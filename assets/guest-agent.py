#!/usr/bin/python3
"""One invocation over Firecracker vsock; image contains no provider or target secrets."""
import ctypes
import errno
import json
import os
import resource
import selectors
import signal
import socket
import struct
import subprocess
import threading
import time

PORT = 8000
MAX_FRAME = 8 * 1024 * 1024


def mount_guest():
    libc = ctypes.CDLL(None, use_errno=True)
    for source, target, filesystem in [(b"proc", b"/proc", b"proc"), (b"sysfs", b"/sys", b"sysfs"), (b"devtmpfs", b"/dev", b"devtmpfs"), (b"tmpfs", b"/tmp", b"tmpfs"), (b"tmpfs", b"/work", b"tmpfs")]:
        if libc.mount(source, target, filesystem, 2 if filesystem == b"devtmpfs" else 2 | 4, b"mode=1777" if filesystem == b"tmpfs" else None) != 0:
            code = ctypes.get_errno()
            if code != errno.EBUSY:
                raise OSError(code, "guest mount failed")


def read_exact(stream, length):
    result = bytearray()
    while len(result) < length:
        chunk = stream.recv(length - len(result))
        if not chunk:
            raise ValueError("incomplete invocation frame")
        result.extend(chunk)
    return bytes(result)


def execute(request):
    if set(request) not in ({"argv", "input", "timeout_ms", "output_bytes", "pids"}, {"argv", "input", "timeout_ms", "output_bytes", "pids", "broker_server_name"}):
        raise ValueError("invalid invocation schema")
    argv = request["argv"]
    if not isinstance(argv, list) or not 1 <= len(argv) <= 128 or any(not isinstance(arg, str) or "\0" in arg or len(arg) > 8192 for arg in argv):
        raise ValueError("argv bound")
    timeout = request["timeout_ms"]
    allowance = request["output_bytes"]
    pids = request["pids"]
    if not isinstance(timeout, int) or not 1 <= timeout <= 60000 or not isinstance(allowance, int) or not 1 <= allowance <= 16 * 1024 * 1024 or not isinstance(pids, int) or not 4 <= pids <= 4096:
        raise ValueError("resource bound")
    data = bytes(request["input"])
    if len(data) > 1048576:
        raise ValueError("input bound")

    def constrain():
        os.setsid()
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
        resource.setrlimit(resource.RLIMIT_NPROC, (pids, pids))
        resource.setrlimit(resource.RLIMIT_NOFILE, (4096, 4096))
        resource.setrlimit(resource.RLIMIT_CPU, (max(1, timeout // 1000 + 1), max(2, timeout // 1000 + 2)))
        os.setgroups([])
        os.setgid(65534)
        os.setuid(65534)
        if ctypes.CDLL(None, use_errno=True).prctl(38, 1, 0, 0, 0) != 0:
            raise OSError("guest no_new_privs failed")

    relay=None
    environment={"HOME": "/work", "PATH": "/usr/bin:/bin", "LANG": "C"}
    if request.get("broker_server_name") is not None:
        name=request["broker_server_name"]
        if not isinstance(name,str) or not name or len(name)>253 or any(c in name for c in "\r\n\0"):
            raise ValueError("invalid broker identity name")
        relay=start_broker_relay()
        environment.update({"SWARM_SOCKET":"/work/broker.sock","SWARM_BROKER_NAME":name,"PYTHONPATH":"/usr/lib/solos"})
    child = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=environment, cwd="/work", preexec_fn=constrain)
    if relay is not None:
        relay.start()
    selector = selectors.DefaultSelector()
    for name, stream in [("stdout", child.stdout), ("stderr", child.stderr)]:
        os.set_blocking(stream.fileno(), False)
        selector.register(stream, selectors.EVENT_READ, name)
    os.set_blocking(child.stdin.fileno(), False)
    if data:
        selector.register(child.stdin, selectors.EVENT_WRITE, "input")
    else:
        child.stdin.close()
    result = {"stdout": bytearray(), "stderr": bytearray()}
    deadline = time.monotonic() + timeout / 1000
    sent = 0
    failed = False
    try:
        while selector.get_map():
            if time.monotonic() >= deadline:
                failed = True
                break
            for key, _ in selector.select(min(0.05, max(0, deadline - time.monotonic()))):
                if key.data == "input":
                    try:
                        sent += os.write(key.fileobj.fileno(), data[sent:sent + 65536])
                    except BrokenPipeError:
                        sent = len(data)
                    if sent == len(data):
                        selector.unregister(key.fileobj)
                        key.fileobj.close()
                    continue
                chunk = os.read(key.fileobj.fileno(), 65536)
                if not chunk:
                    selector.unregister(key.fileobj)
                    key.fileobj.close()
                elif len(result[key.data]) + len(chunk) > allowance:
                    failed = True
                    break
                else:
                    result[key.data].extend(chunk)
            if failed:
                break
        if failed:
            os.killpg(child.pid, signal.SIGKILL)
        exit_code = child.wait(timeout=max(0.1, deadline - time.monotonic()))
    finally:
        if child.poll() is None:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait()
        selector.close()
    return {"exit_code": 124 if failed else exit_code, "stdout": list(result["stdout"]), "stderr": list(result["stderr"])}


def start_broker_relay():
    server=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
    server.bind("/work/broker.sock")
    os.chmod("/work/broker.sock",0o600)
    os.chown("/work/broker.sock",65534,65534)
    server.listen(8)
    slots=threading.BoundedSemaphore(8)
    def forward(source,target):
        count=0
        try:
            while True:
                block=source.recv(min(65536,16*1024*1024-count))
                if not block:break
                count+=len(block);target.sendall(block)
                if count>=16*1024*1024:break
        except OSError:pass
        try:target.shutdown(socket.SHUT_WR)
        except OSError:pass
    def connection(incoming):
        try:
            with incoming,socket.socket(socket.AF_VSOCK,socket.SOCK_STREAM) as outgoing:
                incoming.settimeout(60);outgoing.settimeout(60)
                outgoing.connect((socket.VMADDR_CID_HOST,5001))
                sender=threading.Thread(target=forward,args=(incoming,outgoing));sender.start()
                forward(outgoing,incoming);sender.join(timeout=1)
        finally:slots.release()
    def accept():
        while True:
            incoming,_=server.accept()
            if slots.acquire(blocking=False):threading.Thread(target=connection,args=(incoming,),daemon=True).start()
            else:incoming.close()
    return threading.Thread(target=accept,daemon=True)


def main():
    if os.getpid() == 1:
        mount_guest()
    server = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    server.bind((socket.VMADDR_CID_ANY, PORT))
    server.listen(1)
    print("SOLOS_GUEST_READY", flush=True)
    stream, _ = server.accept()
    with stream:
        stream.settimeout(65)
        length, = struct.unpack(">I", read_exact(stream, 4))
        if length > MAX_FRAME:
            raise ValueError("invocation frame bound")
        request = json.loads(read_exact(stream, length))
        print("SOLOS_GUEST_REQUEST_ACCEPTED", flush=True)
        result = execute(request)
        print("SOLOS_GUEST_RESULT_READY", flush=True)
        body = json.dumps(result, separators=(",", ":")).encode()
        stream.sendall(struct.pack(">I", len(body)) + body)
    server.close()
    # PID 1 remains until the owner confirms VM and cgroup teardown.
    if os.getpid() == 1:
        while True:
            try:
                os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                pass
            time.sleep(0.1)


if __name__ == "__main__":
    main()

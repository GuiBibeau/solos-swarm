"""Bounded tool requests to the host signer; no private credential enters this guest."""
import json
import os
import socket
import struct
import uuid


def read_exact(stream, length):
    data=bytearray()
    while len(data)<length:
        block=stream.recv(length-len(data))
        if not block:raise RuntimeError("host broker channel closed; inspect effect before retry")
        data.extend(block)
    return bytes(data)


def request(path, payload, request_id=None):
    body=json.dumps({"request_id":request_id or str(uuid.uuid4()),"path":path,"payload":payload},separators=(",",":"),ensure_ascii=False).encode()
    if len(body)>1048576:raise ValueError("broker request bound")
    with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as channel:
        channel.settimeout(60);channel.connect(os.environ["SWARM_SOCKET"])
        channel.sendall(struct.pack(">I",len(body))+body)
        length,=struct.unpack(">I",read_exact(channel,4))
        if length>4*1024*1024+1024:raise ValueError("broker response bound")
        response=json.loads(read_exact(channel,length))
        if not 200<=response["status"]<300:raise RuntimeError(f"broker denied tool request: {response['status']}")
        return response["body"]

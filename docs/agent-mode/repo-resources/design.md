# Repository resources: network egress, data mounts and scratch

> **Status, 2026-11-08.** A design, not built. It is the design for
> [#99](https://github.com/GuiBibeau/solos-swarm/issues/99) (a per-repository network egress allowlist) and
> [#100](https://github.com/GuiBibeau/solos-swarm/issues/100) (read-only data mounts and a size-capped scratch per
> Ticket). It changes no code. [Section 6](#6-pr-plan) builds it in ten small PRs, and
> [section 9](#9-decisions-for-the-owner) lists what only the owner can decide.

Grounded in `main` at `364c7ed`, and in read-only measurements on `remote-ai` the same day. [`sketch/`](sketch/) holds
the types and signatures with `todo!()` bodies. It is illustrative and not compiled, like the sketches in the other
design records. It builds on the [long-jobs design](../long-jobs/design.md) for #101 and #102, and it supplies the
per-repository mounts that [repo-models](../repo-models/design.md) decision 9 waits for.

## 0. In one paragraph

A repository Ticket's `run_code` has no network, and it sees only its clone and the global read-only mounts. The
research Tickets need three things: to reach a few named hosts (Modal, GitHub's API, public market-data hosts), to read
the box's data root, and to write builds of up to 150 GB. The recommendation has three parts:

- **The gate.** An HTTP CONNECT proxy inside `agent serve`. Each Ticket reaches it through its own Unix socket. It admits
  only the repository's exact host names, resolves them itself and refuses private addresses. It records every
  connection, and every request for the hosts it inspects. It puts vault keys on requests, so the worker never holds
  them. It caps bytes, and prices them where they cost money.
- **Mounts.** A per-repository list of read-only mounts, with today's rules and a few more.
- **Scratch.** A directory per Ticket. A disk reservation admits it, and the reservation keeps the box above its highest
  floor. The quota covers everything the Ticket writes, its clone included. A host-side accountant measures it without
  following links. Past the quota, the host kills the Ticket's sandboxes and turns file growth off.

Two facts shape all of it. An allowlist limits where a connection goes, not where data ends up: once Modal is on the
list, a worker can send anything it can read to anywhere, because Modal runs the worker's code with the open internet.
And the box had 1,088 GB free on 2026-11-08, so a 950 GB floor leaves 138 GB for everything the agent writes; the
guard's 32 GB margin leaves about 106 GB to admit. That is less than one 150 GB build.

## 1. The problem

### 1.1 Egress (#99)

In the owner's words: research Tickets on the regimes repository need to reach Modal (launch and poll GPU runs, read
the billing meter), GitHub (`gh pr checks` and CI waits) and public data hosts (data.binance.vision, the OKX public
API, the Hyperliquid requester-pays S3 bucket). The ask is a per-repository policy in `forge.repos.<repo>`, or a
broker-mediated proxy: hosts and ports, every request logged, priced where it costs money. Not all or nothing.

**Acceptance.** A regimes Ticket can run `modal app list` and a checksum-verified download from data.binance.vision. It
cannot reach any host off the list. Every request is in the trail.

### 1.2 Mounts and scratch (#100)

Regimes Tickets read `~/.local/share/solos-data` (about 6 TB, read-only) and write builds of 1 to 150 GB. The ask:
`forge.repos.<repo>.mounts`, read-only, and `forge.repos.<repo>.scratch`, a writable directory per Ticket with a
quota, freed by `agent prune`. It must honour the box's free-disk floors: another job needs 950 GB free, and
`health.min_free_gb` is 50.

**Acceptance.** A Ticket can read a Parquet file under the data root and cannot write there. It writes to its scratch
up to the quota, and is refused past it.

### 1.3 What is true today

- `run_code` runs each command in its own bwrap with `--unshare-all`, so it has no network (`src/code.rs:679-691`,
  `configuration.md:169`). Only the browser and the dependency build share the host's network, and a test pins that
  (`src/bwrap.rs:246`).
- `fetch_url` is the worker's only network. In agent mode it has no host list: `FetchGuard::open`
  (`src/broker_fetch.rs:373`) makes a guard with no policy, and the broker uses it (`src/agent/broker.rs:2236`). It
  allows `POST`, `PUT`, `PATCH` and `DELETE` (`src/broker.rs:1354`). So **a hostile worker can already send what it
  reads to any public host**, but only what passes through the model, because a request body is text the model writes.
  A response is cut at 1 MiB (`src/broker.rs:1435-1438`), and an Assignment reads at most 32 MiB (`src/broker.rs:1347`).
- The private-address guard resolves a name once, requires every address to be public, and pins the connection to
  those addresses (`src/broker_fetch.rs:446-475`, `is_public_ip` at `:544`).
- Keyed `fetch_url` honours a `Host` header the worker sets. `admit_headers` refuses one only under a Goal's policy
  (`src/broker_fetch.rs:400-411`), and agent mode has none. The worker's headers are copied onto the request
  (`src/broker.rs:1465-1470`), and the key is judged by the URL alone (`src/vault.rs:442-445`). A test pins the gap
  (`src/broker_fetch_tests.rs:262-265`). The security review confirmed it. The gate must not repeat it, and
  [PR 1](#6-pr-plan) fixes it.
- `executor.run_code.mounts` are global: one list for every Ticket (`src/agent/config.rs:929-933`,
  `src/exposure.rs:62-66`), each bound read-only (`src/code.rs:692-696`). A target is one directory under `/` and not a
  reserved name, and a source is absolute (`src/code.rs:70-106`). No target twice (`src/agent/config.rs:1593-1602`),
  and none at `/pstack` while pstack is on (`src/agent/config.rs:1514-1531`).
- Per-Ticket additions exist already. `Extras` (`src/code.rs:114-127`) carry the dependency cache's binds, `PATH`,
  variables and memory cap. The host builds them (`src/agent/forge/deps.rs:442`) and attaches them to the worker's seat
  (`src/agent/pi_executor.rs:747-748`). That is the seam for per-repository mounts, scratch and egress.
- The writable tree is the Ticket's `/work`, under `<state>/tickets/<id>/` (`src/agent/mod.rs:661-671`). Nothing caps
  its size. `prlimit` caps one file at 256 MiB (`src/code.rs:54`, `:665`) and CPU at 300 s (`:666`), not the total. The
  review confirmed that a worker can fill the shared disk.
- The disk is watched from outside only. `agent health` reads `statvfs` (`src/agent/health.rs:904-916`) every three
  minutes and writes `PAUSE` below `health.min_free_gb` (`src/agent/health.rs:515-543`). A pause stops model calls, not
  a running `run_code` or job.
- The dependency build stops at `deps.max_bytes` by walking its scratch every 2 s (`src/agent/forge/deps.rs:82`,
  `:811-821`, `dir_bytes` at `:851-877`). The review flagged that walk for following links. On Linux,
  `DirEntry::metadata` does not follow a link, but the walk opens each directory again by its path, so a directory
  swapped for a link between the stat and the open is followed. No repository code runs during that build, so nothing
  races it there. A worker's scratch is different: the worker runs while the host walks.
- Repository Tickets run on this host only (`src/agent/pi_executor.rs:711-715`), so none of this reaches remote nodes.
- The Store is at schema 9 (`crates/agent-store/src/schema.rs:4`).

### 1.4 What the box allows

Measured on `remote-ai` on 2026-11-08, as the agent's user `debian`, with read-only commands. Nothing was installed,
and nothing in production was touched.

| What | Measured | Why it matters |
|---|---|---|
| The filesystem of home and state | ext4 on `/dev/nvme0n1p1`, 6.9 TiB, options `rw,relatime,discard,errors=remount-ro` (`df -T ~`, `findmnt`) | No btrfs subvolumes. No project quotas mounted. |
| Free space | 1,088 GB (`df`: 1,062,442,744 KiB available; `solos-data/health.json`: `freeGb: 1088.3`) | 138 GB above a 950 GB floor; about 106 GB after the guard's margin. |
| The data root | `/home/debian/.local/share/solos-data`, mode `0700`, on the same filesystem | A dedicated agent user cannot read it as it is. |
| `fuse2fs`, `squashfuse`, `mksquashfs` | not installed | An image needs an install first. |
| `fuse-overlayfs` | installed | It caps nothing. |
| FUSE | `/dev/fuse` mode `0666`, `fusermount3` setuid root | An unprivileged FUSE mount works. |
| Loop devices | `/dev/loop-control` is `root:disk 0660`; `debian` is not in `disk` | No loop mount without root. |
| `mkfs.ext4` | `/usr/sbin/mkfs.ext4` | An image file can be formatted without root. |
| btrfs tools | absent | |
| User namespaces | `max_user_namespaces` 4,640,511; `unprivileged_userns_clone` 1 | bwrap works, and so would design B. |
| nftables in an unprivileged user and network namespace | `unshare -rn`, then `nft add table` and an output chain with a drop policy: both succeed | Design B needs no root on this box. |
| nftables logging there | `net.netfilter.nf_log_all_netns` is 0 | A `log` rule in such a namespace writes nothing. |
| `slirp4netns` | 1.2.1 installed; `pasta` absent | Design B can reach the host's network unprivileged. |
| cgroup controllers delegated to `user@1000` | `cpu memory pids` | No `io` controller. |
| bwrap | 0.12.0 | |
| A listener in a bwrap sandbox | `127.0.0.2:8443` binds; `:443` is refused, even with `--uid 0 --cap-add CAP_NET_BIND_SERVICE` | Clients must use proxy variables. A transparent `:443` relay is out. |
| `socat`, `curl`, `gh` | in `/usr/bin` | |
| The Modal client | 1.6.1 in `~/research/.venv`. It honours `HTTPS_PROXY` only with `python-socks` (`modal/_utils/proxy_support.py`), which is not installed. grpclib trusts certifi's bundle only (`grpclib/client.py:757-769`) | The venv needs `modal[api-proxy-support]`. An inspected Modal needs the gate's CA bound over certifi's file. |
| Memory | 1,133 GiB | |
| The transfer guard | `transfer-guard.timer`: daily bond0 rx and tx, warn at 750 GB, alert at 1 TB and stop archive backfills | Unbounded downloads would stop the owner's backfills. |
| Addresses | data.binance.vision is `18.66.112.107` (CloudFront). www.okx.com is a Cloudflare name. api.modal.com is `52.0.45.136`, an EC2 address | An address allowlist admits every tenant of those CDNs. |

The 950 GB floor is the owner's figure from #100. I did not find it in the box's unit files, so this design treats it
as a number the owner sets ([decision 5](#9-decisions-for-the-owner)).

## 2. Egress

### 2.1 Design A: the gate

A proxy in `agent serve`, one Unix socket per Ticket, and nothing else that leaves the sandbox.

- The host binds the Ticket's egress directory read-only at `/runtime/egress`; the gate's socket is
  `/runtime/egress/sock` inside the sandbox. A small relay (the solos-swarm binary, bound read-only, as Claude Code's
  sandbox already binds it, `src/exposure.rs:19`) listens on `127.0.0.1:3128` and dials the socket for each client
  connection. Because the directory is bound, not the socket file, a gate restart that replaces `sock` is reached by
  the next connection; a file bind would keep the old inode. Proxy variables point every client at the relay.
- The sandbox keeps `--unshare-all`. It has no route out and no DNS resolver. The socket is the only way out, and the
  gate decides everything. The relay decides nothing: a worker that kills or replaces it gains nothing.
- The gate admits `CONNECT name:port` only for an exact name and port on the repository's list. It resolves the name,
  refuses a private address, and dials the vetted address itself.
- **Tunnel mode** (the default for a public data host): the gate reads the TLS ClientHello, requires its SNI to equal
  the CONNECT name, then copies bytes. It records one row per connection, with bytes each way.
- **Inspect mode** (required for any host that gets a credential, or has method or path rules): the gate ends TLS
  itself with a certificate for that one name, signed by a CA only `agent serve` can use (5.3). It then sees every
  HTTP/1.1 and HTTP/2 request. It refuses a `Host` or `:authority` other than the CONNECT name, applies method and path
  rules, strips the client's credentials, adds vault keys, and records one row per request.

### 2.2 Design B: a network namespace per Ticket, with nftables

For each Ticket, the host makes a user and network namespace it owns. It writes nftables rules there that accept only
the addresses the listed names resolve to, on their ports, and drop the rest. `slirp4netns` connects the namespace to
the host's network. Each sandbox runs inside it: bwrap with `--share-net` after entering it, in a nested user
namespace, so the worker cannot change the rules. Names come from a generated `/etc/hosts`, and no resolver is
reachable.

This is often taken to need root. On this box it does not ([1.4](#14-what-the-box-allows)). Its limits are of
another kind:

- nftables sees addresses, not names. Allowing data.binance.vision's address allows every site CloudFront serves
  there. www.okx.com is on Cloudflare, the same.
- It sees no request. The trail would hold connections by address, not requests by host name. A `log` rule writes
  nothing from that namespace on this box, so logging needs an NFLOG reader of its own.
- It cannot add a credential. The Modal token must be in the sandbox, where the worker holds it.
- CDN addresses change. A long download fails when the pinned set goes stale, and keeping it fresh means resolving
  again and rewriting rules under running work.
- It adds two programs to supervise per Ticket, and a third caller of `share_net` beside the browser and the
  dependency build.

### 2.3 Rejected: the host makes every network call

The host could serve the network as broker tools only: a `fetch_file` that downloads into scratch and checks a
checksum, a `ci_status` that reads GitHub with the App's token. The sandbox would get no socket at all. That is the
strongest design for downloads. But `modal app list` and `modal run` are a CLI the worker runs. Serving them means
running owner-listed commands on the host with the network, which is a sandbox with the whole network (the dependency
build's shape) and no list at all. It fails the acceptance, so it is not compared further. `fetch_file` stays a
possible later tool.

### 2.4 What a hostile worker can send out

Assume the worker is prompt-injected and wants data out. "Today" is `fetch_url` alone. It stays under every design.

| Path out | Today | A, tunnel | A, inspect | B |
|---|---|---|---|---|
| A host off the list | Any public host, through `fetch_url`, only what the model writes | No | No | No by address, but yes for any other name at a listed address |
| Another tenant of a listed host's CDN | n/a | Yes, if the CDN routes by `Host` (domain fronting): the gate checks the SNI and cannot see `Host` inside TLS | No: any other `Host` or `:authority` is refused | Yes |
| A listed host that runs code (Modal) | n/a | Everything the sandbox can read, at full speed | The same, unless path rules allow only read calls | The same |
| A listed host that stores what any account sends (S3 path-style, GitHub with a token the worker brings) | n/a | Yes, with the worker's own credentials | No: the worker's `Authorization` and `Cookie` are stripped, and method and path rules apply | Yes |
| The value of a credential | Never sent to the worker | No credential is added | Never seen. Usable only within the entry's rules. A value echoed in a response is masked | The worker holds it |
| DNS queries | No resolver | None: an unlisted name is never resolved | None | None, if no resolver is reachable |
| Volume | Kilobytes per call, through the model | Up to the Ticket's and the repository's byte caps | The same | Unbounded, unless counters are added |

### 2.5 What an allowlist does and does not stop

**It stops:** reaching any host not on the list, the box's own services, private networks, the Tailscale network and
the cloud metadata address. It stops DNS exfiltration from the sandbox, and casual exfiltration to an attacker's own
server, which is not on the list. In inspect mode it stops credential theft and account confusion. It bounds volume and
cost.

**It does not stop:** data reaching anyone who can read what a listed host receives. Every listed host is a place the
Ticket's data may go. In particular:

- **Modal is the whole internet.** A worker that can launch a Modal function can ship `/work` and every mount to it,
  and the function has the open internet. Path rules can limit Modal to read calls (list apps, read logs, read the
  billing meter), and then Modal runs nothing new. Launching runs, which the owner wants, cannot be told apart from
  exfiltration by any proxy.
- A repository with both a data mount and Modal can send that data anywhere. Mount the narrowest directory that does
  the job.
- A host whose logs or storage an attacker can read, through query strings or uploads, is a channel. Exact names and
  method rules narrow this. Nothing removes it.

What is new is volume. Today a hostile worker can send out only what it pushes through the model. Egress from
`run_code` lets code move gigabytes the model never reads.

### 2.6 Credentials the worker never holds

The vault already has the parts. A key is an item with hosts, a header and a scheme (`src/vault.rs:426-458`). Its
first use on a Ticket can need a person's click (`src/vault/policy.rs:135`), and its use is written to the chain before the
request leaves (`src/agent/broker/secrets.rs:68-81`). The gate uses the same parts, in inspect mode only:

- The repository's entry names keys by the variables the client reads: `keys: {MODAL_TOKEN_ID: modal-token-id,
  MODAL_TOKEN_SECRET: modal-token-secret}`. The sandbox gets each variable set to a placeholder, `vault:<name>`, so the
  client runs. The owner adds two vault keys with headers `x-modal-token-id` and `x-modal-token-secret`, scheme `raw`,
  host `api.modal.com`.
- On each request to the entry's host, the gate removes every client value of those headers, and `Authorization` and
  `Cookie`. Then it puts each key on through `KeyHeader`'s host check, against the request's own URL. `reveal_into`
  today takes a `reqwest::Request`; PR 8 factors the admitted header value out of it into one method both callers
  use, so the check is not written twice.
- The `Host` mistake does not carry over. The gate refuses a request whose `Host`, `:authority` or absolute target
  names anything other than the CONNECT name. It also refuses one where `Host` and `:authority` disagree. And it sends
  upstream to the pinned address with SNI and certificate checked for that name. The CDN therefore sees the name the key
  is for. `Forwarded`, `X-Forwarded-*`, `X-Original-URL` and `X-Rewrite-URL` are removed. The gate's certificate names
  only that host, so an HTTP/2 client cannot reuse the connection for another origin.
- A connection that carries a key writes `Fact::KeyUsed` before its first keyed request leaves.
- A key whose first use on the Ticket needs a person is not put on any request until a person allows it. The gate
  refuses the request with `key_needs_approval` and sends the ask, as keyed `fetch_url` answers 409 and asks
  (`src/agent/broker/secrets.rs:58-65`). The worker retries after the click.
- The gate masks any vault value it finds in a response body byte for byte, at the same length, so gRPC and HTTP
  framing survive. A value the server transforms any other way is not caught. `fetch_url` has the same limit
  (`security.md:83-84`).
- **An API that mints a credential hands it to the worker.** If Modal or another listed API can create a token, a
  worker that calls it gets a durable credential in the response. Path rules are how the owner refuses those calls.
- GitHub needs no vault item. With `github: read`, the gate adds a read-only installation token, minted by the host for
  this one repository, as the forge mints its tokens today (`security.md:131-133`). The sandbox gets `GH_TOKEN` set to
  a placeholder, so `gh` believes it is signed in; the gate removes it and puts the real token on each request.

Tunnel mode adds no credential. A worker that brings its own credential (`GH_TOKEN` copied from an injected page) can
use it in tunnel mode, which is why a host with any credential use must be inspected.

### 2.7 DNS, rebinding and private addresses

- The sandbox has no resolver. A client sends the name in its `CONNECT`, and the gate resolves only names that are on
  the list. An unlisted name causes no DNS query anywhere.
- The gate resolves once per connection, requires every address to be public, and dials those addresses itself, as
  `FetchGuard::pin` does (`src/broker_fetch.rs:446-475`). No second lookup exists for a rebinding to answer.
- `is_public_ip` (`src/broker_fetch.rs:544`) already refuses loopback, RFC 1918, link-local and metadata, shared
  `100.64/10` (Tailscale), unique-local IPv6, multicast, documentation and IPv6 forms that wrap any of those. The gate
  reuses it.
- No private address passes the gate, whatever `executor.fetch.allow_private_networks` says
  ([decision 4](#9-decisions-for-the-owner)).
- An IP literal in `CONNECT` is refused. Names only.
- A redirect a client follows is a new `CONNECT`, and its host must be listed. The gate neither follows nor rewrites a
  redirect.
- Wildcards are refused at config load. `*.modal.run`, `*.s3.amazonaws.com` and `*.cloudfront.net` each name every
  tenant of a shared platform ([decision 4](#9-decisions-for-the-owner)).

### 2.8 Configuration and code

| | A, the gate | B, namespaces |
|---|---|---|
| The owner configures | Per repository: exact names, ports, the mode, keys by variable, method and path rules, a price, byte caps, CA paths for certifi-only clients. Vault keys with `solos-swarm vault add-key`, as today. `python-socks` in the Modal venv. | Per repository: names and ports. Tokens as sandbox secrets. |
| Request trail | Per connection (tunnel), per request (inspect) | Per address, with a netlink reader |
| Credentials | Never in the sandbox (inspect) | In the sandbox |
| New code, estimated | Tunnel: about 1,000 lines (CONNECT, SNI peek, pin, copy, caps, rows, relay). Inspect: about 1,500 more (CA, TLS both sides, HTTP/1.1 and HTTP/2, rules, keys), plus hyper's `http2` and client features, which bring the `h2` crate | About 1,200 lines (namespace holder, slirp4netns supervision, rule generation and refresh, hosts file, NFLOG reader), plus two external programs per Ticket |
| Clients | Must honour proxy variables. Inspect needs the gate's CA in each client's trust | Any client |

**Recommendation: A.** B is simpler for clients and costs no CA plumbing, but it cannot keep a token out of the
sandbox, cannot record requests, and turns every CDN host into its whole CDN.

## 3. Scratch and the disk

### 3.1 The numbers that bound it

The box had 1,088 GB free. With a 950 GB floor and a 32 GB margin, admission sees `1088 - 950 - 32 = 106` GB for
everything the agent writes: clones, mirrors, dependency caches, transcripts and scratch. One 150 GB build does not
fit. A reservation-based design refuses it until space is freed elsewhere. That is the correct answer, and the owner
should expect it ([decision 5](#9-decisions-for-the-owner)).

Writes land in the page cache first, and the box has 1.1 TiB of memory. ext4 counts a delayed write in `statvfs` and in
the file's blocks as soon as it is written, so both measures see it at once. But a writer can outrun the disk by far.
Any measure-and-refuse design can overshoot by what the Ticket writes between two looks, at memory speed.

### 3.2 Five ways to cap a directory without root

| Option | Possible here? | Exact? | Cost and risks |
|---|---|---|---|
| **A host-side accountant**: reserve, walk, kill, refuse | Yes, with no install | No. It overshoots by about one second of writing | Walk cost grows with file count. Must not follow links or lose a race (3.3). Bytes in deleted, open files are invisible until the sandbox ends. About 600 lines. |
| **A preallocated ext4 image on FUSE** (`fuse2fs`) | Needs root once, to install `fuse2fs`. Then unprivileged: `/dev/fuse` is `0666`, `fusermount3` is setuid, `mkfs.ext4` is present | Yes: `ENOSPC` at the cap | A daemon per Ticket. Every write crosses into a user-space ext4, far slower than the kernel's for 150 GB builds. Stale mounts after a crash need `fusermount3 -u`. A sparse image does not shrink after deletes, and a preallocated one spends the 106 GB admission headroom at once. About 500 lines. `squashfuse` is read-only, so it is no scratch. |
| **btrfs subvolume quotas** | No: the root is ext4, and no btrfs tools are installed | Yes | Needs a new filesystem, which needs root. |
| **cgroups or `systemd-run`** | No disk controller exists in cgroup v2. `io` is not delegated here, and it would only throttle bandwidth | n/a | A size-capped tmpfs is exact and charged to the memory cgroup. But it is memory, it dies with each `run_code` (each is a fresh bwrap), and the agent's unit has `MemoryMax=64G` (`scripts/systemd/solos-agent.service:41`). |
| **ext4 or XFS project quotas, or systemd's directory quotas built on them** | No: not mounted with `prjquota`, and turning it on needs root and an unmounted filesystem | Yes | Needs downtime of the root filesystem. |

**Recommendation: the accountant now.** The `fuse2fs` image is the upgrade if the owner needs exact caps
([decision 6](#9-decisions-for-the-owner)). The floor guard (5.4) protects the box either way.

### 3.3 The accountant, made safe

The walk must hold against a worker that is running while it walks:

- **Walk by descriptor, not by path.** Open the root with `O_DIRECTORY | O_NOFOLLOW`. Read each directory through its
  own descriptor, `fstatat(..., AT_SYMLINK_NOFOLLOW)` each entry, and open a subdirectory with
  `openat(dirfd, name, O_DIRECTORY | O_NOFOLLOW)`. A directory swapped for a link between the stat and the open fails
  with `ELOOP` or `ENOTDIR` instead of leading to `/`. `libc` is already a dependency (`src/agent/health.rs:904-916`).
- **Never leave the filesystem.** Skip an entry whose `st_dev` differs from the root's.
- **Open nothing but directories.** A FIFO, a socket or a device file the worker made is stat-ed, never opened, so it
  cannot block the walk.
- **Count what is allocated.** `st_blocks * 512`, so a sparse file costs what it holds. A file with several links
  counts once, by `(dev, ino)`.
- **Never loop.** Remember each directory's `(dev, ino)`. Bound the depth (64), the entries (`max_files`, default one
  million) and the time (2 s). A walk that hits a bound counts as over the quota, with the reason.
- **Hidden bytes.** A file deleted while still open costs disk that no walk sees. The `--fsize` limit bounds each such
  file, the open-file limit bounds how many, and they vanish when the sandbox is killed. The floor guard is the
  backstop for the box.
- **No host writes into the tree.** The host never writes into a scratch, so the review's symlink race on the
  `run_code` file writer (`src/code.rs:787-847`) has no counterpart here. `agent prune` removes a scratch with
  `remove_dir_all` (`src/agent/prune.rs:422-444`), which does not follow links inside the tree, and no sandbox can
  replace the scratch directory itself: it is a mount point there.

The same walk replaces `deps::dir_bytes` (`src/agent/forge/deps.rs:851-877`) and prune's `du`
(`src/agent/prune.rs:514-536`), so the host has one walk, and it is the safe one.

### 3.4 What a hostile worker can do to the disk

| Attack | What happens |
|---|---|
| Write past the quota fast | Overshoot of about one second of writes. Then its sandboxes and detached jobs are killed and growth is off. |
| Race the walk with link swaps | The walk opens by descriptor and does not follow links. |
| A million tiny files | The walk stops at `max_files` and counts the Ticket as over. |
| Hold a deleted file open | Bounded by `--fsize` and the open-file limit, freed at the kill. The floor guard covers the box. |
| Write into `/work` instead, or move files there | Counted. The quota covers `/work` and `/scratch` together, and a move between two mounts is a copy. A Ticket without scratch gets the cap of [decision 5](#9-decisions-for-the-owner). |
| Fill the disk below the floor anyway | The floor guard kills every sandbox and detached job, and pauses. |

## 4. Per-repository mounts

There is one design. It is the global list's rules, per repository:

- `forge.repos.<repo>.mounts` takes the same `{target, source}` entries as `executor.run_code.mounts`, checked by the
  same `Mount::validate` (`src/code.rs:83-106`). Each is bound read-only, after `/work`, as the dependency binds are
  (`src/code.rs:705-710`).
- No target twice, across the global list and the repository's. A repository cannot shadow a global mount.
- No target at `/pstack` while pstack is on, and none at `/scratch`, which joins the reserved names
  (`src/code.rs:70-72`).
- Each source is made canonical at load, and must be an existing directory.
- A source must not be `/`, the agent user's home, or a directory that holds the state directory, the config's
  directory, the vault or its key, or the GitHub App's key. Today's checks look the other way: they refuse a secret under
  a visible path (`src/vault.rs:684-698`). They do not refuse a mount of a whole home, which would show
  `~/agent/tokens`. This rule applies to the global list too ([decision 7](#9-decisions-for-the-owner)).
- Each source joins `Exposure` (`src/exposure.rs:98-128`), so the vault, the trail key, the browser's runtime and the
  App's key are refused under it, as for global mounts.

The owner mounts the narrowest directories a repository needs: `phoenix_decoded/v1`, not the data root, which also
holds `private/`.

A link inside a mounted tree resolves inside the sandbox. An absolute link to `/home/debian/...` finds nothing there,
because the home is not bound.

`ops/harden-units` (merged, #117) makes the agent's user configurable and documents the move to a user of its own.
Until that migration is run, the agent still runs as `debian`; after it, the data root (mode `0700`, owner `debian`)
is closed to the agent. Root must open it once, read-only ([decision 8](#9-decisions-for-the-owner)).

## 5. Recommended design

### 5.1 Configuration

```yaml
disk:                                     # new, read by agent serve
  floor_gb: 950                           # never take free space below this; required with scratch, at least health.min_free_gb
  margin_gb: 32                           # default 32: the floor guard acts this far above the floor
forge:
  kind: github
  repos:
    GuiBibeau/solos-markov-regime-switching-solana-perps:
      about: Regime-switching research on Solana perps (Python, Modal).
      base: main
      owes: pull_request
      mounts:                             # read-only, as executor.run_code.mounts
        - {target: /data, source: /home/debian/.local/share/solos-data/phoenix_decoded/v1}
        - {target: /venv, source: /home/debian/research/.venv}
      scratch:
        max_gb: 60                        # the quota for /work and /scratch together; 1 to 1024
        # max_files: 1000000
        # keep_hours: 24                  # after the Ticket ends, then agent prune frees it
      egress:
        ticket_gb: 100                    # bytes both ways, per Ticket
        daily_gb: 200                     # per repository per UTC day
        ca_paths: [/venv/lib/python3.13/site-packages/certifi/cacert.pem]
        hosts:
          - host: data.binance.vision     # tunnel mode, port 443
          - host: www.okx.com
          - host: api.modal.com           # inspect mode, because it has keys
            keys: {MODAL_TOKEN_ID: modal-token-id, MODAL_TOKEN_SECRET: modal-token-secret}
            # paths: ["/modal.client.ModalClient/AppList"]   # read-only Modal
          - host: api.github.com
            github: read                  # the host's read token for this repository
            methods: [GET, POST]
            paths: ["/repos/GuiBibeau/solos-markov-regime-switching-solana-perps/**", "/graphql"]
```

`egress.hosts` entries:

| Field | Default | Meaning |
|---|---|---|
| `host` | required | An exact name, lowercase ASCII (punycode for others). No wildcard, no IP literal. Once per repository. |
| `ports` | `[443]` | Ports a `CONNECT` may name. |
| `mode` | `inspect` when `keys`, `github`, `methods`, `paths` or `price` is set, else `tunnel` | `tunnel` with any of those is refused at load. |
| `keys` | none | Variable name to vault key name. Each key's hosts must include `host`. Checked when `agent serve` opens the vault, and refused there. |
| `github` | none | `read`: a read-only installation token for this repository. Needs `forge: github`. |
| `methods`, `paths` | any | Request rules. A path is a glob over the path without its query. |
| `price` | none | `{per_gb_microusd}`: what each GB moved costs, held and settled on the Ticket's account. |

`/venv` is the research venv; the interpreter it holds is a link into the global `executor.run_code.uv_root`, so that
mount is needed too, as for any mounted venv (`configuration.md`). `scratch` and `mounts` are as in sections 3 and 4.
`scratch.max_gb` covers the Ticket's `/work` and `/scratch` together: a quota on `/scratch` alone would be escaped by
moving files into the clone. `disk.floor_gb` must be at least `health.min_free_gb`. A config with `scratch` but no
`disk` section is refused, so the owner states the floor once ([decision 5](#9-decisions-for-the-owner)).

Egress rules are read from the running config, not recorded in a Ticket's terms. `ForgeTerms` are a promise about the
work. Egress is a permission, and the owner's narrowing should reach running Tickets at the next restart. A detached
job resolves the repository's rules at `job_start` and keeps them for its life, as the
[long-jobs design](../long-jobs/design.md) needs; a config change applies to the next job.

### 5.2 What a Ticket's sandbox gets

All of it arrives through `Extras`, built by the host, never by a worker. A detached job's sandbox gets the same
three, resolved at `job_start`.

- Each repository mount, read-only at its target.
- With `scratch`: `<state>/tickets/<id>/scratch` writable at `/scratch` (a new `Bind::Writable`). `--fsize` becomes the
  quota, capped at 64 GiB, instead of 256 MiB, because builds write bigger files. With the quota over, it becomes 0.
- With `egress`:
  - `/runtime/egress` (a read-only directory, holding `sock`) and `/runtime/solos-swarm` (read-only). The command
    becomes `/runtime/solos-swarm egress-relay --listen 127.0.0.1:3128 --socket /runtime/egress/sock -- /usr/bin/bash -c CODE` (a new `Extras.wrapper`).
  - `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and their lowercase forms set to `http://127.0.0.1:3128`, `NO_PROXY`
    empty, and `NODE_USE_ENV_PROXY=1`.
  - With any inspected host: a bundle of the system roots and the gate's CA, kept in `<state>/egress/`, bound over
    `/etc/ssl/certs/ca-certificates.crt` and over each of `ca_paths`. `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`,
    `CURL_CA_BUNDLE`, `AWS_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS` and `GIT_SSL_CAINFO` point to it.
  - Each `keys` variable, set to `vault:<name>`.

The sandbox keeps `--unshare-all`. The `share_net` test (`src/bwrap.rs:246`) stays as it is.

### 5.3 The gate, step by step

1. **Socket.** At a Ticket's first Assignment, when its repository has `egress`, the host makes
   `<state>/egress/<ticket>/` (`0700`) and opens `sock` in it. That is the Ticket's identity: no token, and no Ticket
   can reach another's. A detached job a Ticket starts uses the same socket, which stays open for the Ticket until
   its last job ends. The sandbox binds the directory, so after a restart the host replaces `sock` in it and the
   relay's next connection reaches the new socket.
2. **CONNECT.** At most 8 KiB of head, within 10 s. The gate refuses: an IP literal; a name not exactly on the list
   (lowercased, trailing dot removed); a port not on the entry; an ended Ticket; a shut pause gate; a Ticket past
   `ticket_gb`; a repository past `daily_gb`; more than 32 open connections for the Ticket. A plain `http://` request
   is refused with "use https". A refusal is a `403` with the reason in the body and in `X-Solos-Egress`, a row, a log
   line like `fetch_url`'s (`src/agent/broker.rs:2265`), and one Note per host per Assignment, so the worker reads why
   on its next turn, as job ends reach it (`runtime.md:271-274`).
3. **Resolve and pin.** As in 2.7.
4. **Tunnel.** Answer `200`. Read the ClientHello (at most 16 KiB, within 5 s). Require an SNI equal to the name, and
   refuse a ClientHello that carries Encrypted Client Hello (or the older ESNI), whose real name is hidden: on a CDN
   that serves ECH, such as Cloudflare, it would front for any other tenant. Forward it, then copy both ways with
   counters, a 5-minute idle timeout and the byte caps.
5. **Inspect.** Answer `200`. Finish TLS with a one-day leaf certificate for exactly that name. The CA is made once
   and kept at `<state>/egress/ca.pem` (mode 0600, refused wherever a sandbox can read it, as the vault is), so a
   restart reuses it and a running detached job's TLS keeps working. It is name-constrained to the hosts in the
   config, so a stolen key could sign for nothing else. A host-list change makes a new CA, and the host rewrites the
   bundle in place, same inode, so running sandboxes trust the new root without a restart. Serve HTTP/1.1 or HTTP/2
   as the client offers. For each request, apply 2.6, the method and path rules, and the byte caps. Send it upstream
   over a pinned rustls connection, verified against the public roots, so the sandbox's trust settings never decide
   upstream trust. Re-encode each request through hyper rather than pass bytes, which removes request smuggling
   between the two sides. Refuse an `Upgrade` (WebSocket) and HTTP/2's extended CONNECT, which would turn an inspected
   connection back into opaque bytes.
6. **Records.** A row when a connection opens (write-ahead, so a crash still leaves the attempt), completed when it
   closes, with bytes each way. In inspect mode, a row per request: method, path without its query, status, bytes, and
   the names of the keys used.
7. **Price.** On an entry with `price`, the gate holds the price of the next 256 MiB on the Ticket's account before it
   copies them, settles what moved, and closes the connection when a hold is refused. Model calls are held the same
   way (`crates/agent-store/src/ledger.rs:156-217`), with the provider `egress:<host>`.
8. **End.** A stop stops the Ticket's detached job units, then closes its socket and connections; an end closes
   them after the last job ends. An `agent serve` restart reopens each live Ticket's socket with the same CA, and
   in-flight connections drop, a detached job's included; the job reconnects. The
   [long-jobs design](../long-jobs/design.md) accepts that cut.

Modal's own spend is not bytes. #102 meters it with `modal billing` and settles it on the Ticket's account. The gate
gives #102 a switch: `Gate::cut(ticket, host)` closes and refuses one host for one Ticket, the tripwire #102 asks for.
Until then, the Modal keys' default `--approval first-use` makes a person allow Modal once per Ticket, and a Ticket
whose channels cannot ask cannot use it.

### 5.4 Scratch, from reservation to free

1. **Admit.** At a Ticket's first Assignment, under one lock in `agent serve`:
   `headroom = free - floor - margin - sum(open reservations' quota - used)`. With `headroom >= max_gb`, the host makes
   the directory (`0700`), records the reservation and binds it. A restart reads an open reservation back from the
   table and binds the same directory. Otherwise there is no `/scratch`, and the worker's prompt says why, with the
   numbers, as for unavailable dependencies (`src/agent/pi_executor.rs:327-331`). A later Assignment tries again.
2. **Measure.** The accountant walks a Ticket's `work/` and `scratch/` every second while one of its sandboxes or
   detached jobs runs, and every minute otherwise. It records `used`, `files` and `peak`.
3. **Over.** Past `max_gb` or `max_files`, or on a walk that hit a bound, the host kills the Ticket's sandboxes and
   background jobs with the jobs' kill (freeze, kill the namespace, then bwrap: `src/code.rs:1353-1374`), stops its
   detached job units, and marks the reservation `over`. Its commands then run with `--fsize=0`: `rm`, `du` and `ls`
   work, and a write that grows a file fails. The killed command's receipt and a Note say so. Back under the quota, it
   is `reserved` again.
4. **Release.** At the Ticket's end, the unused part of the reservation is released. The files stay.
5. **Free.** `agent prune` removes `scratch/` of a Ticket that ended more than `keep_hours` ago, and marks it `freed`.
   A resumed Ticket whose scratch was freed is admitted again, empty, and its worker is told.
6. **Floor guard.** `agent serve` reads `statvfs` every second. Below `floor + margin`, it writes `PAUSE` with its own
   first line, kills every running sandbox and dependency build, stops every detached job unit, and refuses new ones
   and new gate connections. It lifts its own pause above `floor + margin` plus a tenth of the margin. `agent health`
   neither lifts nor overwrites a `PAUSE` it did not write (`src/agent/health.rs:515-558`).

### 5.5 Store changes

Two tables. Each takes the next free schema version when it lands; the merge-queue, long-jobs and repo-models designs
also claim bumps, so no fixed pair is assumed here. Each bump makes an older build refuse the file, as every bump does
(`crates/agent-store/src/lib.rs:78-81`).

- **The `scratch` table**: one row per Ticket. Columns: repository, quota, file cap, used, files, peak, state
  (`reserved`, `over`, `released`, `freed`), the last measure's time, and the creation, release and free times. Only
  `agent serve` writes it, and prune marks `freed`.
- **The `egress` table**: one row per connection, and one per request in inspect mode. Columns: Ticket, Assignment,
  connection id, time, host, port, mode, method, path, status, verdict (`allowed` or `refused`), reason, bytes each
  way, cost and key names. Only the gate writes it. `agent prune` removes a Ticket's rows with its transcripts
  ([decision 9](#9-decisions-for-the-owner)).

Nothing changes in `agent-core`. `KeyUsed`, `Note` and the ledger's holds exist already. The rows are adapter records,
like the ledger's, not Engine events. A connection or a request is not a decision.

### 5.6 Broker tools and grants

- The network is not a tool. It is a property of a repository Ticket's `run_code` and job sandboxes. The Router has no
  `run_code`. The dream is not a repository Ticket. A swarm cannot name a repository (`repository-tickets.md:18`).
  None of them gets egress, mounts or scratch.
- A new worker tool, `ticket_resources`, read-only. It returns the repository's hosts (mode, ports, methods, paths,
  price), the Ticket's bytes and caps, its last 20 refusals, its mounts, and the scratch's quota, use and state. A
  repository Ticket holds it when its repository has `egress`, `mounts` or `scratch`. It joins the registry,
  `extensions/broker.ts` and `tests/fixtures/agent-tools.json`, which the drift tests hold equal.
- The worker's system prompt gets one paragraph: the hosts; that clients use the proxy variables; that credentials are
  added by the host and are never visible; and the scratch's path and quota.
- A Claude Code worker runs its commands through the same `run_code`, so it gets the same.

### 5.7 Watchdog, status and dashboard

- **Watchdog**, pure over the Store (`src/agent/watchdog.rs:326`), with three new signals:
  - `scratch_over`: a reservation over for 10 minutes;
  - `disk_floor`: a `PAUSE` with the floor guard's mark, told at once rather than after the hour a held pause waits;
  - `egress_refusals`: 20 refusals on one Ticket within an hour, a worker probing or a list that is too short.
- **`agent status`**: a disk line (free, floor, reserved) and, per Ticket, scratch use and egress totals.
- **`solos-swarm agent egress <ticket> [--json]`** prints the Ticket's rows, refused ones included. It is the trail
  the acceptance asks for.
- **Dashboard**: `TicketSnapshot` (`src/agent/publish/snapshot.rs:108`) gains `egress` (per host: connections,
  requests, bytes, cost, refusals; and the latest 40 rows with paths, never queries) and `scratch` (quota, used, peak,
  state). The Ticket page shows both. "What is published" in `production.md` names them.

### 5.8 Docs

- `configuration.md`: `disk`, and `mounts`, `scratch` and `egress` under repository settings.
- `repository-tickets.md`: new sections "Network" and "Data and scratch".
- `security.md`: an "Egress" section, with 2.4 and 2.5 in short, the Modal statement included.
- `runtime.md`: the worker tool table says what `run_code`'s network is.
- `production.md`: the floor and margin, the scratch prune timer, the CA, the data root's permissions after the user
  migration, and `python-socks` in the Modal venv.
- `how-to/repositories.md`: the regimes example of 5.1.
- `status.md`.

### 5.9 How it is tested

Builds and tests run on the box, through the shared test helper.

- **Gate units**, with resolver and dial seams like `FetchGuard`'s (`src/broker_fetch.rs:478-538`):
  - exact match, case and trailing dot; port refused; IP literal, wildcard and private address refused;
  - no lookup for an unlisted name (the seam counts lookups);
  - SNI absent or different, refused;
  - `Host`, `:authority` or an absolute target naming another host refused, over HTTP/1.1 and HTTP/2;
  - routing headers removed; the worker's `Authorization` removed;
  - a key reaches only its host; a value echoed back is masked at the same length;
  - byte caps close; price holds and settles; a pause refuses new connections.
- **Gate integration on the box**, with a real bwrap `run_code`, local hyper TLS servers and a test CA:
  - `curl` and Python `requests` through the relay reach a listed name, and an unlisted name is refused with a row;
  - `getent hosts example.com` fails in the sandbox;
  - a raw socket to `1.1.1.1:443` fails;
  - an HTTP/2 gRPC echo server sees the injected metadata;
  - a second Ticket's socket is not in the sandbox.
- **Accountant**: a link loop; a link to `/`; a directory swapped for a link while a thread walks; hard links; a sparse
  file; a deep tree; `max_files`; the time bound.
- **Scratch**: reservation arithmetic with a fake `statvfs`; a write past the quota is killed and the next write fails
  while `rm` works; back under the quota writes work; prune frees and marks `freed`; the floor guard trips and lifts
  only its own `PAUSE`.
- **Mounts**: config tests (reserved, duplicate, `/scratch`, `/pstack`, a home or an ancestor of the state directory,
  a missing source); a golden argv test; on the box, a read works and a write fails with `EROFS`.
- **The owner's acceptance**, with the `verify-agent-mode` skill on the box: a regimes Ticket runs `modal app list` with
  the keys in the vault; downloads a data.binance.vision file and its `.CHECKSUM` and passes `sha256sum -c`; is refused
  `curl https://example.com`; and `agent egress <ticket>` lists all three. Then it reads a Parquet file under `/data`,
  fails to write there, and writes `/scratch` to its quota and is refused past it.

### 5.10 Dependencies on other work

Two of the branches named when this was first written have merged, and three designs now sit beside this one on `main`.
This says what each gives, or needs, and what changed now that it has landed.

- **`feat/cargo-deps`** (in flight) adds per-repository dependency mounts. Both features add per-Ticket binds through
  `Extras` and its seat (`src/agent/pi_executor.rs:747-748`). PR 3 lands after it and builds on its `Extras` merging,
  or adds the merging if it has none. Any top-level target it mounts joins `RESERVED_TARGETS`, so a repository mount
  cannot take it. Its disk cap should use PR 2's walk.
- **`ops/harden-units`** (merged, #117): it makes the agent's user configurable and documents the migration, and the
  unit keeps bwrap working with `RestrictNamespaces=user mnt pid net ipc uts cgroup` and no `PrivateUsers=`.
  `<state>/egress/` sits under the state directory, which the unit already allows. The migration to a dedicated user
  still has to open the data root to it (decision 8).
- **`fix/outsider-spend`** (merged, #112): a turn starts only for an allowed person's input or a Note, and an
  outsider's text can only ride in such a turn, so a priced or keyed request is always part of a turn an allowed
  person or a Note started. The gate adds no outsider check of its own; it spends only through the Ticket's ledger
  account, with the same holds as model calls.
- **`feat/worker-file-tools`** (in flight): if its tools write into a Ticket's tree from the host, they must use the
  accountant's no-follow opens and count against the quota.
- **`fix/memo-trust`** (in flight): no dependency. A memo is text, and text grants no host.
- **[Long jobs](../long-jobs/design.md)** (#101, #102; a design, not built): it needs egress for a detached job's
  sandbox, resolved from the repository's policy at `job_start` and fixed for the job's life, plus a trail row per
  connection. Because the gate lives in `agent serve`, a restart cuts a job's connections; that design accepts the
  cut and the job reconnects. A job keeps the Ticket's socket and scratch, and a restart reads the reservation back
  from the `scratch` table. This design gives it `Gate::cut`, the tripwire #102 asks for. Its decision 3 puts a Modal
  token in the job's sandbox so a Modal app can read it; an inspected Modal entry keeps it in the gate instead. If
  both land, use one: the entry's `keys`, with nothing in the sandbox, is this design's recommendation.
- **[Merge queue](../merge-queue/design.md)** (#108; a design, not built): a queue Ticket is a repository Ticket, so
  its worker gets the repository's egress, mounts and scratch like any other, and each resolve round's edits count
  against the scratch quota.
- **[Repo models](../repo-models/design.md)** (#103; a design, not built): its decision 9 keeps the research
  repository's data out of the global mounts until mounts are per repository. This design is that mechanism.
- **[Host ops](../host-ops/design.md)** (#104; a design, not built): it frees disk under the floor from an owner's
  approved plan, which is how a refused reservation gets space without adding a disk. It also needs the same
  `ops/harden-units` migration. There is no other interaction.

## 6. PR plan

Each PR leaves `main` shippable and is checked by its own tests on the box.

| # | PR | What it adds | The check |
|---|---|---|---|
| 1 | `fetch_url` refuses a worker's `Host` | `admit_headers` refuses `Host` with or without a policy. `KeyHeader::reveal_into` puts no key on a request whose `Host` differs from its URL. Skipped if a fix lands first. | A keyed fetch with `Host: other` is refused with `header_not_allowed`, and the other vhost never sees the key. The test at `src/broker_fetch_tests.rs:262` flips. |
| 2 | One safe walk | `fs_usage::measure(root, limits)`, by descriptor (3.3). It replaces `deps::dir_bytes` and prune's `du`. | The accountant tests of 5.9. The deps and prune suites pass unedited. |
| 3 | Per-repository mounts | `forge.repos.<repo>.mounts`, the rules of section 4 (applied to global mounts too, if decision 7 says so), `Exposure`, `/scratch` reserved. After `feat/cargo-deps`. | Config tests, a golden argv, read and `EROFS` on the box. |
| 4 | Scratch | `disk`, `scratch`, the `scratch` table, admission, `Bind::Writable`, `--fsize`, the accountant task, over and `--fsize=0`, release, prune's `scratch` item and its timer, the prompt, `ticket_resources` (scratch half). | The scratch tests of 5.9, and a V9 file migrating. |
| 5 | The floor guard | The 1 s `statvfs` task, kill and refuse, its `PAUSE`, the `disk_floor` signal. | A fake `statvfs` trips and lifts it. `agent health` keeps its hands off the guard's `PAUSE`. |
| 6 | The gate, tunnel mode | `egress` config (tunnel fields), the socket, `egress-relay`, the proxy variables, CONNECT, pin, SNI, caps, the `egress` table, Notes, `ticket_resources` (egress half), the prompt paragraph. | Gate units and the box integration for tunnel mode. On the box: a checksum-verified data.binance.vision download, and a refused `example.com`. |
| 7 | Trail and views | `agent egress`, `agent status` lines, the dashboard's `egress` and `scratch`, `egress_refusals` and `scratch_over`, the docs of 5.8. | Golden output of `agent egress`. Snapshot tests. Watchdog unit tests. |
| 8 | Inspect mode | The CA, leaf certificates, TLS on both sides, HTTP/1.1 and HTTP/2, request rules, routing headers, `keys` (the header method factored out of `reveal_into`), `KeyUsed`, masking, the CA bundle and `ca_paths`. | The inspect tests of 5.9. The owner's `modal app list` on the box. |
| 9 | GitHub reads | `github: read`: a host-minted read token for the one repository. | A fake GitHub API sees the token only on the repository's paths. `gh pr checks` through the gate on the box. |
| 10 | Priced bytes, and S3 later | `price` holds and settles. Then requester-pays S3: the gate signs with SigV4 using a vault-held AWS key (decision 10). | Ledger rows per 256 MiB. A refused hold closes the connection. A SigV4 test vector. |

PRs 2 to 5 (#100) and 6 to 10 (#99) are independent tracks. PR 1 goes first because PR 8 reuses `reveal_into`.

## 7. Migration

Nothing changes for a repository without the new fields. Turn it on one repository at a time, after the two steps that
live outside the config.

1. **The data root, once.** After the `ops/harden-units` migration ([Run the agent as its own
   user](../production.md#run-the-agent-as-its-own-user)), root grants the agent user read access to the mounted
   subdirectories ([decision 8](#9-decisions-for-the-owner)). As the agent's user, `ls` one of them. Until then a
   mount binds a directory the sandbox cannot read.
2. **The Modal venv, once.** As the agent's user, install `modal[api-proxy-support]` into `~/research/.venv` for
   `python-socks`, and put certifi's bundle in `ca_paths`. Run `modal app list` through the proxy by hand: it is the
   first half of the acceptance.
3. **The floor, then scratch.** Add `disk` before any `scratch`; config load refuses a `scratch` without a `disk`
   section. Set `floor_gb` to the highest floor any other job needs (950 here), never below it, and `margin_gb: 32`;
   the guard then acts at 982. Watch `agent status` for a day. Then add `scratch` to the first repository. An
   `agent serve` restart is what reads a new config.
4. **Then egress, in two steps.** Add the tunnel hosts first (`data.binance.vision`, `www.okx.com`), restart, and
   check a download and the trail. Then add the inspected hosts with `keys` (`api.modal.com`) and the CA bundle, and
   run the acceptance. A host that shows up in `egress_refusals` is added only after reading why.
5. **Store and rollback.** `agent serve` migrates to the new tables at start, and `scripts/agent-deploy` backs up
   `agent.db` before every deploy. An older binary refuses a newer file, so a rollback is the backup and the previous
   release, not the previous release alone. Removing `egress`, `scratch` or `mounts` from the config and restarting
   returns that repository to today's sandbox. Files already in a scratch stay until `agent prune --only scratch`
   frees them after `keep_hours`, or by hand. A mount removed from the config leaves nothing on the host.
6. **Lowering a quota.** Lowering `scratch.max_gb` below a Ticket's use marks it over at the next walk and kills its
   sandboxes, so drain the Ticket first. Raising it is free while the reservation still fits under the floor.
7. **Watch the first week.** `scratch_over`, `disk_floor` and `egress_refusals` in Discord, `agent egress` on the
   regimes Ticket, and free space in `agent status`.

## 8. Not covered

- The 300 s CPU cap on `run_code` and jobs (`src/code.rs:666`, `runtime.md:255`) still stops long builds. The
  [long-jobs design](../long-jobs/design.md) (#101) covers long jobs.
- Modal's spend is the long-jobs design's (#102). This design gives it the switch.
- `fetch_url` keeps any public host and its write methods. Narrowing it for repositories with egress is a separate
  change.
- No UDP or QUIC (the sandbox has no route for them), no plain `http://`, and no WebSocket in inspect mode.
- A single file over the sandbox's `--fsize` fails even when the directory quota is larger: 256 MiB on a Ticket
  without scratch, at most 64 GiB with it. The per-file limit and the quota are separate.
- Freeing disk under the floor. The accountant refuses a reservation and the guard pauses; neither deletes anything.
  The [host-ops design](../host-ops/design.md) (#104) covers an approved delete.
- Remote nodes. Repository Tickets run on this host only.

## 9. Decisions for the owner

Each has a recommended answer.

1. **Egress mechanism.** A gate inside `agent serve` (A), or a network namespace per Ticket with nftables (B)?
   **Recommended: A.** B cannot keep a credential out of the sandbox, cannot record requests, and turns a CDN host
   into its whole CDN. The one thing it offers, a mechanism that outlives `agent serve`, the long-jobs design accepts
   losing ([5.10](#510-dependencies-on-other-work)).
2. **Tunnel or inspect for the public data hosts.** Tunnel records connections, not requests, and a CDN that routes
   by `Host` can reach its other tenants; inspect needs the gate's CA in every client. **Recommended: tunnel for the
   public data hosts, and inspect wherever a credential, a method rule or a path rule applies.**
3. **Modal.** Allowing Modal to launch runs means a worker can send everything it can read anywhere, because a Modal
   function runs its code with the open internet. **Recommended: allow it on the regimes repository, with both Modal
   keys at `--approval first-use`, so a person allows Modal once per Ticket until #102 caps its spend.** The narrow
   alternative, path rules limited to read calls, refuses `modal run`.
4. **Names and addresses.** **Recommended: exact names only, no wildcard, no IP literal, and never a private address
   through the gate, whatever `executor.fetch.allow_private_networks` says.** A wildcard over a shared platform
   admits every tenant, and a private address reaches the box, the Tailscale network and the metadata service.
5. **Disk limits and the floor guard.** The floor, where 150 GB builds go, what the guard stops, and the `/work` cap.
   **Recommended:** `disk.floor_gb: 950` (the other job's floor) and `margin_gb: 32`; the guard stops every sandbox,
   detached job and dependency build, and pauses; and `executor.run_code.work_max_gb: 32` for a Ticket without scratch.
   With 1,088 GB free, the guard acts at 982 and admission sees about 106 GB, so no 150 GB build fits until space is
   freed or a disk is added.
6. **The accountant or an exact image.** **Recommended: the accountant now, with no install.** It can overshoot by
   about a second of writes. If that is not acceptable, install `fuse2fs` and add an `image` mode later; exact quotas
   on the root filesystem would need a project-quota mount and downtime.
7. **Mount sources that hold the agent's secrets.** Apply the new refusal to the global `executor.run_code.mounts`
   too? **Recommended: yes.** Today a global mount of the home would show `~/agent/tokens`.
8. **How the dedicated user reads the data root.** **Recommended:** root grants the agent user a group or an ACL on
   the mounted subdirectories, once, during the `ops/harden-units` migration. The alternative is a root-made read-only
   bind of each directory under `/srv`.
9. **Egress volume and its trail.** `ticket_gb: 100` and `daily_gb: 200` per repository, and the connection and
   request rows pruned with the Ticket's transcripts after `--keep-days`. **Recommended: as stated.** The box's
   transfer guard stops the owner's archive backfills at 1 TB a day, and ledger rows stay forever, as today.
10. **Requester-pays S3.** The gate signs with SigV4 using a vault-held AWS key, or the host hands the worker
    presigned URLs, credentials it holds until they expire. **Recommended: the gate signs, in a later PR.**
11. **Scratch after the Ticket ends.** **Recommended: `keep_hours: 24`, with an hourly `agent prune --only scratch`
    timer**, so the daily 04:30 prune does not keep an ended Ticket's files for up to two days.

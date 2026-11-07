# Run commands on your own machine

A node is a machine that runs the commands of the Tickets bound to it, such as a laptop with a checkout or a box with
a dataset. The agent keeps every session. The node only runs commands and answers. It dials out to the agent, so it
needs no open port. How nodes behave is in [Remote nodes](../runtime.md#remote-nodes).

The node must be Linux with `/usr/bin/bwrap` and `/usr/bin/prlimit`, and the agent's executor must be `pi`.

1. On the agent's host, make the agent's certificate and note its fingerprint:

   ```sh
   solos-swarm agent init-nodes --dir /srv/agent/node-tls
   # wrote /srv/agent/node-tls/cert.pem and key.pem; pin this fingerprint in each node's agent.fingerprint:
   # sha256:3f1c...
   ```

2. On the machine, make its certificate:

   ```sh
   solos-swarm node init --dir ~/.config/solos-node/tls
   # sha256:9a07...
   ```

3. In agent.yaml, pin the node and say where sessions run by default:

   ```yaml
   nodes:
     listen: 0.0.0.0:7443           # where nodes dial in
     cert: node-tls/cert.pem        # relative to agent.yaml
     key: node-tls/key.pem
     allow:
       laptop:
         fingerprint: "sha256:9a07..."
         about: "the solos-swarm checkout at /repo"    # the Router reads this
   executor:
     kind: pi
     # ...
     environment: local             # or laptop: where a session runs when the Router names nowhere
   ```

4. On the machine, write node.yaml, with its mounts and the secrets its commands may use:

   ```yaml
   name: laptop                     # its name in nodes.allow
   agent:
     address: agent.example.com:7443
     fingerprint: "sha256:3f1c..."  # from agent init-nodes
   cert: tls/cert.pem               # relative to node.yaml
   key: tls/key.pem
   work_root: /home/me/.local/state/solos-node/work   # each Ticket's /work is a directory here
   sandbox:
     mounts:                        # read-only, each one directory under /
       - {target: /repo, source: /home/me/src/solos-swarm}
       - {target: /data, source: /home/me/datasets/phoenix}
     # uv_root, concurrency (4), memory_bytes (4 GiB), job_max_secs (86400), bwrap, prlimit
   secrets:
     env:                           # variables: the file's text without its trailing newline
       GITHUB_TOKEN: secrets/github-token
     files:                         # read-only at /secrets/<name>
       aws: /home/me/.aws/credentials
   ```

   A secret is at least 8 bytes. It never goes to the agent: the node redacts what a command prints, by exact match.
   A value that the command transforms (encodes, splits, reverses) is not caught, and neither is one cut at the
   output limit.

5. Run the node, under systemd or in a terminal:

   ```sh
   solos-swarm node serve --config node.yaml
   ```

   It dials the agent, says which node it is, and serves until SIGINT or SIGTERM. It dials again whenever the
   connection ends.

The Router names a node with `environment` on a session accept. Otherwise the Ticket gets `executor.environment`.
Every field of both files is in the [configuration reference](../configuration.md#nodes).

Connect only machines whose mounts and secrets you would hand to the agent host. A compromised agent host can run any
command on every connected node: see [Remote nodes](../security.md#remote-nodes).

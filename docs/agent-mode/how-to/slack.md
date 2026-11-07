# Connect Slack

The agent connects out to Slack over Socket Mode, so the app needs no request URL and the host exposes nothing. How
the agent behaves in Slack is in [Slack](../channels.md#slack).

## Create the Slack app

1. Create an app at <https://api.slack.com/apps>, **From scratch**, in your workspace.
2. Turn on **Socket Mode**. Slack offers to create an app-level token. Give it the `connections:write` scope. This is
   the `xapp-` token.
3. Under **OAuth & Permissions**, add these bot token scopes:
   - `app_mentions:read`, for mentions.
   - `chat:write`, to reply.
   - `channels:history` for public channels, `groups:history` for private ones, and `im:history` for direct
     messages. These carry the message events and let a repeated reply search its thread (`conversations.replies`).
4. Turn on **Event Subscriptions** and subscribe to the bot events `app_mention` and `message.channels`, and to
   `message.groups` and `message.im` if you need them.
5. Turn on **Interactivity & Shortcuts**. Under Socket Mode no request URL is needed. The Approve and Stop clicks
   arrive on the socket.
6. Install the app to the workspace and copy the bot token (`xoxb-`).
7. Invite the bot to each channel it should listen to (`/invite @your-app`). Note each channel's id, which is at the
   bottom of the channel details (`C...`). A direct message with the bot has a `D...` id that you can list too.
8. Write each token to its own file, readable only by the agent's user. Each start reads both files and deletes them,
   so provide fresh files for every start.

## Configure the agent

```yaml
policy:
  people: [slack:U012ALICE]            # who may ask for work
  approvers: [slack:U012ALICE]         # who may approve a budget above the threshold
  auto_approve_microusd: 2000000
sources:
  - kind: slack
    channels: [C0123OPS]
    app_token_file: secrets/slack-app-token   # xapp-..., relative to this file
    bot_token_file: secrets/slack-bot-token   # xoxb-...
channels:
  - kind: slack                        # replies with the slack source's bot token
router: {kind: scripted}
executor: {kind: scripted}
budgets: {session_microusd: 500000}
```

Start `agent serve` with this config. Every field is in the [configuration reference](../configuration.md#sources).
Start with `accept_mode: observe`, the Policy's default, so inferred work is judged and recorded but never started.

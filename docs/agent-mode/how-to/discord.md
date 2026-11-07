# Connect Discord

The agent connects out to Discord's Gateway and posts over REST, so the host exposes nothing. How the agent behaves
in Discord is in [Discord](../channels.md#discord).

## Create the Discord application

1. Create an application at <https://discord.com/developers/applications> with **New Application**.
2. On the **Bot** page, turn on **Message Content Intent** under **Privileged Gateway Intents**. The agent needs no
   other privileged intent.
3. Select **Reset Token** and copy the token, which Discord shows once. This is the bot token.
4. Under **Installation** (or **OAuth2**, **URL Generator**), install with the scope `bot` and the permissions
   **View Channels**, **Send Messages**, **Send Messages in Threads** and **Read Message History**. As one invite URL,
   with the application's id:

   ```
   https://discord.com/oauth2/authorize?client_id=APPLICATION_ID&scope=bot&permissions=274877975552
   ```

   Open it and pick the server. A channel with permission overrides needs the same permissions for the bot's role.
5. Leave **Interactions Endpoint URL** empty on the **General Information** page. With one set, Discord sends button
   clicks to that URL, and the agent's buttons do nothing.
6. Turn on Developer Mode (**User Settings**, **Advanced**) to copy ids. Right-click a server and choose **Copy Server
   ID**, a channel and choose **Copy Channel ID**, or a person and choose **Copy User ID**. A direct message with the
   bot has a channel id too, the last part of its address, `discord.com/channels/@me/<id>`.
7. Write the bot token to a file readable only by the agent's user. Each start reads it and deletes it, so provide a
   fresh file for every start.

## Configure the agent

```yaml
policy:
  people: [discord:333333333333333333]     # user ids who may ask for work
  approvers: [discord:333333333333333333]  # who may approve a budget above the threshold
  auto_approve_microusd: 2000000
sources:
  - kind: discord
    guilds: [111111111111111111]           # every channel of these servers
    channels: [222222222222222222]         # these channels, their threads, and DM channels
    token_file: secrets/discord-token      # relative to this file
channels:
  - kind: discord                          # replies with the discord source's bot token
router: {kind: scripted}
executor: {kind: scripted}
budgets: {session_microusd: 500000}
```

Write ids quoted or unquoted. A message enters when its server is in `guilds`, its channel is in `channels`, or it is
in a thread of a channel in `channels`. A direct message enters only when its channel id is in `channels`.

## Fix a start that fails

`agent serve` exits with an error that names the fix:

- Close code 4004, or a 401 from `GET /gateway/bot`: Discord refused the token. Reset it and write the new one to the
  token file.
- Close code 4014: **Message Content Intent** is off. Turn it on (step 2).
- Close code 4011: the bot is in too many servers for one connection. The agent does not shard.

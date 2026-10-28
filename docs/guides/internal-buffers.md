# Internal Buffers

Internal buffers are buffers managed by Halloy instead of an IRC server.

You can open them from the user menu or command bar. You can also
[add them to the sidebar](/configuration/sidebar#internal-buffers).

- **Channel Monitor** shows messages from all joined channels in one place.
  [Configuration](/configuration/channel-monitor)
- **Highlights** shows messages that highlighted you.
  [Configuration](/configuration/highlights)
- **Logs** shows Halloy log messages.
  [Configuration](/configuration/logs)
- **File Transfers** shows your active and completed file transfers.
  [Configuration](/configuration/file-transfer)
- **Channel Discovery** shows channels available on a server.
  [Click action configuration](/configuration/actions#click_channel_discovery)
- **Config Editor** lets you edit Halloy's configuration inside Halloy.
- **Search** searches messages across all servers, channels and queries.
  See [Search](#search).

You can change how internal buffers open with the
[`open_internal` action](/configuration/actions#open_internal).

## Search

Open Search from the command bar, the user menu, the
[`search` key binding](/configuration/keyboard), a nickname's context menu
("Search messages"), or with `/search [text]`.

To search only the focused channel or query, use the
[`search_buffer` key binding](/configuration/keyboard) or "Search in focused
buffer" in the command bar. In a server buffer, they search that server, but
not the networks of a bouncer. A channel's context menu has "Search in this
channel".

Search matches whole words, and the last word as you type it, so `rel` finds
"release". Results are newest first, and new matching messages are added at the
top while the results are scrolled to the top. Click the channel name on a
result to jump to the message.

| Syntax                   | Matches                                                |
| ------------------------ | ------------------------------------------------------ |
| `word`                   | Messages with `word`                                   |
| `"exact phrase"`         | Words next to each other                               |
| `-word`, `-"phrase"`     | Messages without `word` or the phrase                  |
| `error OR warning`       | Either word                                            |
| `from:nick`              | Messages from `nick`                                   |
| `-from:nick`             | Messages not from `nick`                               |
| `in:#channel`            | Messages in `#channel` (or in a query with a nick)     |
| `server:libera`          | Messages on a server                                   |
| `is:channel`, `is:query` | Only channels or only queries                          |
| `after:2024-05-01`       | From that day on                                       |
| `before:2024-05-01`      | Before that day                                        |
| `on:2024-05-01`          | On that day                                            |
| `/regex/`                | Messages matching the regular expression               |
| `case:sensitive`         | Words, phrases and the regex match case                |

- `OR` must be uppercase. It binds tighter than the words around it, so
  `error OR warning disk` needs `disk` and one of the other two.
- Repeat `from:`, `in:` or `server:` to match any of several values.
- `server:` takes a server's name, its bouncer network's name or its bouncer's
  name, ignoring case. `server:bouncer/network` matches only that bouncer
  network, and `server:name/` only the server named `name`, not a bouncer
  network with that name.
- Days are in your local time zone.
- A `/regex/` ends at a slash followed by a space or the end of the search, so
  `/etc/hosts` is a word. In a `/regex/`, write `\/` for a slash.
- The value of any `key:` can be quoted, as in `server:"My Network"`.

The filter bar below the search field sets `server:`, `is:`, `after:` and
`case:` for you: it edits the text, so you can also change those filters by
typing. For exact days, type `after:`, `before:` or `on:`.

A `/regex/` and `case:sensitive` are checked after the search index, so each
page looks through a limited number of messages. When a page stops early,
"Load more" keeps searching older messages.

Only messages sent by users are searched. History from before Halloy stored it
in SQLite becomes searchable once its buffer has been opened.

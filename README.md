# A basic, token conscious Redmine MCP server

An MCP server for the Redmine project management API. The binary is static. Running it needs no Node and no extra runtime.

Token cost is the constraint. `get_issue` defaults to a summary and does not fetch journals. A summary of a heavily edited issue is 768 bytes. The uncapped payload of that issue was 286 KB. List rows drop descriptions and empty custom fields. Counts and cross-tabs are computed in this process, so those rows never reach the model. An unknown status or custom field value is an error that lists the valid names. Redmine answers that typo with zero rows, which looks like a real empty result.

There is one write tool. It stays off unless you enable it. A description change requires a note, and both go out in one PUT so Redmine files them as one journal entry.

## Build

Install Rust from <https://rustup.rs/>. From this directory:

```bash
cargo test
cargo build
```

`cargo clean` removes the `target` directory. It does not uninstall Rust.

## Install

```bash
cargo install redmine-dev-mcp
```

That puts the binary in `~/.cargo/bin`. From a checkout of this repo, `cargo install --path .` does the same thing. To skip the copy, run `cargo build --release` and use the full path to `target/release/redmine-dev-mcp` in the commands below. Apps started from the Dock often do not have `~/.cargo/bin` on `PATH`, so use that full path for Claude Desktop.

You don't start the server yourself. The MCP client launches it when a session opens and talks to it over stdin and stdout. The process starts serving at once and checks the key with `/users/current.json` in the background, logging the login on stderr. A rejected key (HTTP 401) stops the process. Any other failure, such as no network yet, is logged as a warning, and each tool call tries Redmine again.

## Use with Claude Code

Register the server with `claude mcp add`. Everything after `--` is the command Claude Code runs.

```bash
claude mcp add redmine-dev \
  -e REDMINE_URL=https://redmine.example \
  -e REDMINE_API_KEY=your-api-key \
  -- redmine-dev-mcp
```

Your API key is on your Redmine account page, under "API access key". On Planio, `REDMINE_URL` is your account URL, such as `https://yourcompany.plan.io`.

Start a new session and run `/mcp`. `redmine-dev` should show as connected, with its tools listed. If it shows as failed, the stderr log in that view usually says why: a missing variable, a bad URL, or a rejected key.

### Scope

By default the server is registered for you, in this directory only. `--scope` changes that:

| Scope | Where it applies | Stored in |
| --- | --- | --- |
| `local` (default) | You, in this project | `~/.claude.json`, under the project |
| `user` | You, in every project | `~/.claude.json` |
| `project` | Everyone who clones the repo | `.mcp.json` in the repo root |

For a server you use across repos, `--scope user` is usually what you want.

### Keeping the key out of the config

`claude mcp add` stores `-e` values in `~/.claude.json` as written. To keep the key in your shell environment instead, export it from your shell profile and pass a reference. The single quotes stop the shell from expanding it, so Claude Code stores `${...}` and fills it in when it starts the server:

```bash
export REDMINE_API_KEY_VALUE=...   # in ~/.zshrc or similar

claude mcp add redmine-dev \
  -e REDMINE_URL=https://redmine.example \
  -e 'REDMINE_API_KEY=${REDMINE_API_KEY_VALUE}' \
  -- redmine-dev-mcp
```

Claude Code started from a terminal sees the variable. An app opened from the Dock doesn't read your shell profile, so the key arrives empty and every call fails with "Redmine rejected the API key".

### Removing it

```bash
claude mcp remove redmine-dev
```

Pass the scope it was added with when it's not `local`, for example `claude mcp remove redmine-dev -s user`. `claude mcp get redmine-dev` shows the scope. The same command removes any other MCP server by name, such as an older Redmine server this one replaces. Start a new session afterwards.

### Sharing it with a team

A `project` scope server lives in `.mcp.json`, which gets committed. Keep the key out of it. Claude Code expands `${VAR}` from each person's environment, so the file can name the variables without holding them:

```json
{
  "mcpServers": {
    "redmine-dev": {
      "command": "redmine-dev-mcp",
      "env": {
        "REDMINE_URL": "https://redmine.example",
        "REDMINE_API_KEY": "${REDMINE_API_KEY}"
      }
    }
  }
}
```

Each person then sets `REDMINE_API_KEY` in their shell profile. Claude Code asks for approval the first time it sees a project server.

### Enabling writes

Writes are off by default. To allow them for a few named issues, add the variables to the same command:

```bash
claude mcp add redmine-dev \
  -e REDMINE_URL=https://redmine.example \
  -e REDMINE_API_KEY=your-api-key \
  -e REDMINE_READONLY=false \
  -e REDMINE_WRITE_ISSUES=101,102 \
  -- redmine-dev-mcp
```

`claude mcp add` will not overwrite an existing server. Run `claude mcp remove redmine-dev` first, or use a second name such as `redmine-dev-write`.

### Other settings

Any variable from the configuration table below goes in as another `-e`. A config file needs an absolute path:

```bash
-e REDMINE_CONFIG=/Users/you/.config/redmine-dev/instance.json
```

To see the server's debug log, add `-e RUST_LOG=redmine_dev_mcp=debug`. It logs each request path, never the key.

## Use with Claude Desktop

Edit `~/Library/Application Support/Claude/claude_desktop_config.json` on macOS, or `%APPDATA%\Claude\claude_desktop_config.json` on Windows. Use the binary's full path, because Claude Desktop doesn't read your shell profile:

```json
{
  "mcpServers": {
    "redmine-dev": {
      "command": "/Users/you/.cargo/bin/redmine-dev-mcp",
      "env": {
        "REDMINE_URL": "https://redmine.example",
        "REDMINE_API_KEY": "your-api-key"
      }
    }
  }
}
```

Restart Claude Desktop after saving.

## Configuration

`REDMINE_URL` and `REDMINE_API_KEY` are required. The key is sent as `X-Redmine-API-Key`.

| Variable | Default |
| --- | --- |
| `REDMINE_URL` | required |
| `REDMINE_API_KEY` | required |
| `REDMINE_READONLY` | `true` |
| `REDMINE_WRITE_PROJECTS` | empty, meaning every project |
| `REDMINE_WRITE_ISSUES` | empty, meaning every issue |
| `REDMINE_TIMEOUT_MS` | `15000` |
| `REDMINE_MAX_JOURNALS` | `50` |
| `REDMINE_MAX_NOTE_CHARS` | `6000` |
| `REDMINE_MAX_DESCRIPTION_CHARS` | `20000` |
| `REDMINE_MAX_DETAIL_CHARS` | `160` |
| `REDMINE_MAX_ATTACHMENT_BYTES` | `102400` |
| `REDMINE_MAX_LIST` | `100`, which is also the most Redmine returns per page |
| `REDMINE_MAX_SCAN` | `5000` |
| `REDMINE_SCAN_TTL_MS` | `60000` |
| `REDMINE_CATALOG_TTL_MS` | `600000` |
| `REDMINE_CONFIG` | optional JSON file |

Values are trimmed, and a blank value counts as unset. `REDMINE_READONLY` stays on unless the value is `false`. `REDMINE_WRITE_PROJECTS` and `REDMINE_WRITE_ISSUES` are comma-separated allowlists. A project entry matches the numeric id, the identifier from the project URL, or the name. Use the identifier: it survives a rename. A malformed issue id stops startup. Accepted forms are `123`, `#123`, and an issue URL on `REDMINE_URL`. A link to another Redmine is refused rather than read as an id here. The issue allowlist is checked before any request.

`REDMINE_CONFIG` points at an optional JSON file and must be an absolute path. MCP clients start servers from directories you don't control, so a relative path would break.

```json
{
  "issue_prefix": "ticket",
  "fields": {
    "product": 2,
    "customer": "Customer",
    "severity": 36
  },
  "status_groups": {
    "in-review": ["Review: Waiting", "Review: In progress"]
  }
}
```

`issue_prefix` is the slug accepted beside a numeric id, as in `ticket-123`. Omit it for none. Each value under `fields` is a custom field id, or the field's exact label. Those keys are named filters on `list_issues`, `count_issues` and `facet_issues`, columns you can add to a list, and values in the `group_by` enum. With no file, filters go through `custom_fields` only, and `group_by` still accepts any field key, checked against the catalog when the tool runs. `status_groups` are extra names accepted anywhere a status is. Unknown keys in the file fail at startup, as do field keys that clash with a built-in parameter such as `status` or `assignee`, and groups named `open` or `closed`. Write a field id as a number: `"2"` is read as a label. A field or status the instance does not have, or a status group member that isn't an exact status name, fails when the catalog loads.

### Field names

The server keeps a catalog of the instance's statuses, trackers, priorities, saved queries and issue custom fields. It loads on the first call that needs it and is kept for `REDMINE_CATALOG_TTL_MS`. Custom fields come from `/custom_fields.json` when the key is an admin. For other keys they come from `/projects.json?include=issue_custom_fields` (Redmine 4.2 and later). On older versions the server samples one issue per tracker.

Every custom field gets one key, and `get_issue` and the filters use the same one. A configured field uses its configured key. Any other field uses its label in lowercase, with other characters turned into `_`. When that is empty, a built-in name, a configured key, or already taken by a field with a lower id, the field gets `cf_<id>` instead. Every field also answers to `cf_<id>`.

If the catalog can't load, `get_issue` still returns the issue. Configured fields keep their keys, every other field shows as `cf_<id>`, and a "Field names unavailable" line says why. While the catalog is down, issue reads don't retry it more than once every 30 seconds. If the config file doesn't match the instance, the line says so, and the full list of valid names goes to the server log. `list_issues`, `count_issues`, `facet_issues` and `list_meta` fail instead, and the error lists the valid names. A custom field added while the catalog is cached is picked up on the next miss, at most once a minute.

In a batch, configured fields get columns. With no config file, a field needs a value on at least half the rows to get one.

The base URL may include a path prefix such as `https://redmine.example/redmine`. Requests keep that prefix.

The API key only goes to `REDMINE_URL`. Redirects are followed within the same host, port and path prefix, and from http to https. A redirect anywhere else, such as an SSO login page, fails with an error naming the target. If you see that error, `REDMINE_URL` is probably not the address the API answers on.

TLS trusts both the bundled Mozilla roots and the system store, so an internal CA installed on the machine works.

GETs that receive HTTP 429 are retried twice, waiting on `Retry-After` and never longer than 10 seconds. Writes are not retried.

## Tools

Markdown is the default. JSON is compact, with no indentation. List-shaped JSON is columnar: `columns` names each field once, and `rows` holds the values.

| Tool | Job |
| --- | --- |
| `get_issue` | One id, or up to 20. `summary` is the default and does not fetch journals. `description`, `journals`, and `full` add those parts, with the issue text in code fences. A description longer than `REDMINE_MAX_DESCRIPTION_CHARS` comes back in windows that end on a line break. Pass `description_next_offset` as `description_offset` with `view=description` for the next window. An id can be `123`, `#123`, an issue URL, or the configured slug. A batch is one request and includes closed issues. Ids Redmine doesn't return are listed as not found, not treated as an error. `journals_since` takes `YYYY-MM-DD` or a UTC timestamp. |
| `list_projects` | Projects the key can see: id, identifier, name, and updated, in either format. |
| `search` | Full text. Returns ids and clipped titles. `snippet_chars` defaults to 0. `project_id` limits the search to one project. `scope` takes Redmine's values: `all`, `my_projects`, `bookmarks`, `subprojects`. |
| `get_attachment` | Metadata. `fetch=true` downloads text types only, under the byte cap, and returns the text in a code fence. A file that isn't valid UTF-8 comes back with the bad bytes replaced and is marked as such. |
| `list_issues` | Compact table. Filters take names. Defaults to open issues, newest updated first. Descriptions stay off unless you ask, and are capped at 1,000 characters a row. A custom field value no issue in scope has is an error that lists the values in use. A custom field filter Redmine would ignore (hidden from the key, not enabled on the project, or not a filter) is refused instead of returning unfiltered rows. `query_id` can't be combined with other filters, because Redmine ignores them. Unknown arguments are refused. |
| `count_issues` | How many issues match. Same filters as `list_issues`, one number. A custom field value no issue in scope has is an error that lists the values in use. |
| `facet_issues` | Same filters, a histogram or a two-way cross-tab. Counts only. Above `REDMINE_MAX_SCAN` it refuses instead of returning a partial total. `top` keeps the N largest groups on each axis, default 25, and rolls the rest into `(other)`. Without a `status`, Redmine counts open issues only, and the output says so. An issue with no value for a field is counted under `(none)`; an unchecked checkbox counts as `0`, not `(none)`. |
| `list_meta` | Statuses, trackers, priorities, custom fields, saved queries, and, given a project, its categories and members. Cached with the catalog. |
| `update_issue` | A note, a description find and replace, and checklist changes, in one journal entry. A note is required when the description changes. Checklist items are addressed by the ids `get_issue` shows. `dry_run` previews without writing. Off unless `REDMINE_READONLY=false`. See below for how it confirms a write. |

### How update_issue confirms a write

Redmine answers 204 even when the workflow drops a field your role cannot edit. After every write the tool reads the issue back. The description check ignores line endings, because Redmine stores the description as CRLF, so an LF body coming back as CRLF still counts as the same text. The tool also checks the checklist, and that the note is in the newest journal entry.

If the request was sent and then something else fails, a timeout or a failed read-back, the error says the write may have landed and gives the issue link. Look before you retry. A retry posts the note again. A refusal Redmine answers outright, 401, 403, 404, or 422, means nothing was saved.

A private note sent with a description or checklist change is filed as two entries. The private note is one. A public entry holding the changes is the other, and anyone who can see the issue sees that diff. The dry run and the result both say so.

`expected_updated_on` refuses the write when the issue has changed since you read it. The check and the PUT are separate requests. Someone else can edit the issue in between, and this write overwrites theirs. The REST API has no conditional update.

## Logs

Logs go to stderr. stdout belongs to MCP.

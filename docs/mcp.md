# MCP Server

`rustgis-cli mcp` runs RustGIS as a [Model Context Protocol](https://modelcontextprotocol.io)
server, so an AI agent such as Claude Code, Claude Desktop, Cursor, VS Code or Codex can make
maps with it. The agent opens data, runs any RustGIS command or geoprocessing tool, reads
attribute tables, looks at the map as an image and looks things up in this documentation.

```sh
rustgis-cli mcp --sample                 # the World Atlas sample
rustgis-cli mcp --data parcels.geojson   # your own data (--data more than once, or --project p.rgis)
rustgis-cli mcp --yes                    # also allow file and network access
rustgis-cli mcp --attach [--yes]           # the desktop app that is already open (its MCP Relay)
```

The server talks over standard input and output, so you don't start it yourself: you tell your
agent how to start it (below), and the agent runs it. Each agent gets its own project, held in
memory until the agent saves it.

## Adding it to an agent

Claude Code:

```sh
claude mcp add rustgis -- rustgis-cli mcp --sample
```

Codex:

```sh
codex mcp add rustgis -- rustgis-cli mcp --sample
```

Claude Desktop (`claude_desktop_config.json`), Cursor (`~/.cursor/mcp.json`) and other clients
that use an `mcpServers` list:

```json
{
  "mcpServers": {
    "rustgis": {
      "command": "rustgis-cli",
      "args": ["mcp", "--sample"]
    }
  }
}
```

VS Code (`.vscode/mcp.json`):

```json
{
  "servers": {
    "rustgis": {
      "type": "stdio",
      "command": "rustgis-cli",
      "args": ["mcp", "--sample"]
    }
  }
}
```

If the agent can't find `rustgis-cli`, give its full path (see [Installation](installation.md)).
Then ask, for example:

- "Which 5 countries have the largest GDP? Show them on the map."
- "Buffer the cities by 200 km, colour the result by population and show me the map."
- "Load ~/data/parcels.geojson, label the parcels with their owner and export the map to
  parcels.png." (this reads and writes files, so it needs `--yes`)

## Managing the open desktop app

By default each agent gets its own project in memory. To drive the project that is already
open in the desktop app instead, start its relay listener first: in RustGIS, open Command
Search (Ctrl+K), run **MCP Relay** — the status bar confirms `MCP relay listening on
127.0.0.1:8767`. The listener remains active until the app closes.

```sh
rustgis-cli mcp --attach                 # the open app on 127.0.0.1:8767
rustgis-cli mcp --attach 9000 --yes      # another port, with file and network access
```

```json
{
  "mcpServers": {
    "rustgis-app": {
      "command": "rustgis-cli",
      "args": ["mcp", "--attach", "--yes"]
    }
  }
}
```

The relay speaks the same protocol as above, so every tool works — `view_map` shows the open
map, `run_command` runs on it, each run one undo step. Two things change: the project is
yours, not the agent's own (opening, replacing, undoing and saving act on the open project),
and the listener is off until you start it. It binds 127.0.0.1 only, on port 8767 unless the
command names another (`ui.mcpRelay {"port": 9000}`); GeoLibre's 8766 is untouched. File and
network commands still need `--yes`, under the same policy as the private server.

## Tools

| Tool | What it does |
| --- | --- |
| `list_layers` | The map: its layers (name, type, feature count, fields, visibility, selection), basemap, centre and scale |
| `view_map` | The map as a PNG image, at the current view |
| `search_commands` | Finds RustGIS commands by words: their ids, ribbon locations and parameters |
| `run_command` | Runs a command by id with JSON parameters; each run is one undo step. The result names the layers it added (`layersAdded`) and what the command said beside it (`notices`), including any parameters it ignored (`layer.moveDown ignores 'steps' (it reads layer)`) |
| `search_tools` | Finds geoprocessing tools by words |
| `describe_tool` | A geoprocessing tool's parameters |
| `run_tool` | Runs a geoprocessing tool; its output is added to the map as a new layer (with `layersAdded` and `notices` as for `run_command`) |
| `read_features` | A layer's attribute rows, optionally only those matching a where clause, sorted |
| `search_docs` | Searches this documentation and returns the best matching sections |
| `read_doc` | Reads a documentation page as Markdown, or one section of it |

These are the [AI Assistant](user-guide/assistant.md)'s tools, plus `view_map` and the
documentation tools. Every page of this documentation is also a resource
(`rustgis://docs/cli.md`, `rustgis://docs/user-guide/maps.md`…) that a client can attach to a
conversation.

## What the agent may do

Your agent asks you before each tool call, and the server adds its own limits:

- Commands that read or write files, connect to an address on the internet or run a plugin's
  command run only when the server was started with `--yes`, as does opening a project from
  text (it could bring in web layers). Without it, the agent can still work with the data you
  gave the server and with the built-in sample, basemaps and geoprocessing. `view_map` draws
  the web tile layers the project you opened already has, fetching their tiles.
- Plugin management, scripts, tasks, models and the GeoLibre runner are never run from here.
- Opening, replacing and undoing in the project are allowed: the project is the agent's own,
  not the one open in the desktop app.

Installed [plugins](plugins.md) load first, as with the other subcommands (`--no-plugins` skips
them). Notes such as "MCP server ready" go to standard error; standard output carries only the
protocol.

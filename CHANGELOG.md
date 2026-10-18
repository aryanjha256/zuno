# Changelog

What changed in each release of Zuno, newest first. Add to **Unreleased** as you work; cutting a
release (`scripts/release.sh`) turns it into the new version's section, and that section becomes
the top of the GitHub release page.

## Unreleased

## 0.5.7 - 2026-10-19

### Added
- Toasts for errors and finished background work; errors stay until dismissed.
- Ctrl+P finds requests by URL as well as name.

### Fixed
- Latin-1, Windows-1252 and UTF-16 responses show as text, not a hex dump.
- JSON starting with a byte-order mark parses.
- macOS and Windows system proxy settings are used.
- Ctrl+P stays fast with large collections.
- Ctrl+W asks before closing an unsaved curl-import or restored tab.

## 0.5.6 - 2026-10-16

### Fixed
- **Postman import:** v2.0 collections import their auth correctly instead of sending an empty
  password; an OAuth 2.0 block that already holds a token sends it; Basic auth goes to the Auth
  tab, so `{{variables}}` in it still resolve.
- **OpenAPI import:** each request carries the spec's auth, with the key as one variable you fill
  once in the environment the import selects. Form and multipart bodies (Stripe, Twilio) arrive
  with their fields. A spec split across several files says so instead of importing half-empty.
- **curl import** reads flags the way curl does: `-XPOST`, `-sSL`, `-H'X: y'`, `-d @file.json`,
  `--data-urlencode`, `--oauth2-bearer`, `-T`, and options Zuno does not use no longer take the
  URL's place. Commands from Chrome's "Copy as cURL (cmd)" on Windows and `curl.exe` from
  PowerShell import correctly.
- **Bundles** keep each request's own settings and recreate empty folders. **Postman export**
  keeps a query string typed into the URL.
- Server-sent event streams that arrive in large bursts parse hundreds of times faster.

## 0.5.5 - 2026-10-13

### Added
- **Dynamic variables:** `{{$guid}}`, `{{$randomUUID}}`, `{{$timestamp}}`, `{{$isoTimestamp}}`
  and `{{$randomInt}}`, made fresh for every occurrence at send.
- **GraphQL connection params:** a Connection tab whose JSON is sent as the subscription's
  `connection_init` payload, so subscriptions can authenticate.
- The first-run request is Zuno's own repository on GitHub, sendable with no setup.

### Changed
- Save and the ⋯ menu sit in the URL row after Send, and the request and response tab strips
  scroll instead of pushing their buttons off the edge.

## 0.5.4 - 2026-10-13

### Added
- **Git in the side panel:** initialise a repository, see changes and markers in the tree, commit,
  publish, sync (fetch, rebase, push) with conflict handling, clone a workspace from a git URL,
  and discard a file's changes.
- **Changes tab:** a field-by-field diff of a request against its last commit, unsaved edits
  included.
- Tooltips on every icon-only control.

## 0.5.3 - 2026-10-09

### Added
- **Global history:** every send is kept per workspace; Alt+H searches it and reopens a request
  with its response. History is a side-panel view next to Collections.

### Fixed
- Postman export keeps the Auth tab.

## 0.5.2 - 2026-10-07

### Changed
- Shortcuts shown as keycaps everywhere, a polished picker and palette, and request actions
  gathered into Save plus a ⋯ menu.

## 0.5.1 - 2026-10-06

### Added
- **Network tab:** timing, connection and certificate details, and the request exactly as sent.

### Changed
- The collection panel shows method pills, kind icons and indent guides, and the URL bar is one
  field with a separate Send button.

## 0.5.0 - 2026-10-05

### Added
- `{{` completes variable names, and every `{{variable}}` is highlighted by whether it resolves,
  with a hover showing its value and where it came from.

## 0.4.9 - 2026-10-04

### Added
- **Auth tab:** none, Basic or Bearer, with the derived header shown but never saved.

## 0.4.8 - 2026-10-04

### Added
- Header values are suggested for their header.
- OpenAPI specs written in YAML import.

## 0.4.7 - 2026-09-30

### Added
- Hang up a bidirectional gRPC call, and send a file as a binary WebSocket frame.

## 0.4.6 - 2026-09-30

### Added
- **GraphQL schema support:** fetch a schema from the server, then get completion, validation
  with underlined problems, variable checking and a browsable schema under the query.
- Choosing a gRPC method fills an empty message with its request skeleton.

## 0.4.5 - 2026-09-29

### Added
- **macOS and Windows:** Zuno.app for Apple Silicon and a Windows installer, both installed and
  updated by one command, with native window controls and each platform's shortcuts.
- A tarball and an AppImage for Linux distributions without apt.
- A cookie viewer, with one cookie jar shared by every request.

## 0.4.4 - 2026-09-24

### Added
- **Copy as code:** a request as a ready-to-run snippet for curl and other languages and tools.

## 0.4.3 - 2026-09-24

### Added
- **gRPC**, in all four call shapes.

## 0.4.2 - 2026-09-21

### Added
- **Server-sent events**, and GraphQL over SSE and over `graphql-transport-ws`.

## 0.4.1 - 2026-09-20

### Added
- **WebSocket** requests.

## 0.4.0 - 2026-09-20

### Added
- Export a collection as a **Zuno bundle**, which keeps everything, or as a **Postman
  collection**.

## 0.3.0 - 2026-09-19

### Added
- **GraphQL** as a request kind of its own.

## 0.2.9 - 2026-09-13

### Added
- A hex viewer for binary responses, HTML responses as readable text, saving binary responses
  to disk, and a byte-by-byte body diff.

## 0.2.8 - 2026-09-12

### Changed
- Header editing improvements.

## 0.2.7 - 2026-09-11

### Fixed
- The release checksum file.

## 0.2.6 - 2026-09-11

### Added
- The one-line install script, and a hint when an update is available.

## 0.2.5 - 2026-09-11

### Added
- Client certificates and custom root CAs, a workspace proxy, and actions on the tab strip.

## 0.2.4 - 2026-09-10

### Added
- A response timeline, a resizable collection panel, context menus, and formatting and
  minifying a JSON request body.

## 0.2.3 - 2026-09-09

### Added
- **Postman import:** collections (schema 2.x), environments, and test scripts recovered as
  assertions and captures.

## 0.2.2 - 2026-09-08

### Added
- A collection runner, request chaining, an environment editor, and default settings for new
  requests.

## 0.2.1 - 2026-09-05

### Added
- Workspaces, unsaved-change markers on tabs, and collapse and expand all in the collection panel.

## 0.2.0 - 2026-09-03

### Added
- A collection panel with folders (create, rename, move, duplicate, delete to trash), and
  **OpenAPI import**.

## 0.1.7 - 2026-09-02

### Added
- The side panel and collection browser, and an application menu.

## 0.1.6 - 2026-08-30

### Added
- Find and replace in the request editor, syntax highlighting, and horizontal scrolling for wide
  requests and responses.

## 0.1.5 - 2026-08-29

### Added
- Response search, row selection with copy value and copy path, full text editing in every
  field, copy as curl, and global keybindings.

## 0.1.4 - 2026-08-25

### Changed
- Maintenance release.

## 0.1.3 - 2026-08-23

### Added
- Separate tabs for the response body and headers.

### Fixed
- Imported curl requests no longer follow redirects or accept compression unless the command
  asked for it, and variables are substituted into form and multipart bodies.

## 0.1.2 - 2026-08-22

### Added
- Form, multipart and binary request bodies, and saving and copying responses.

## 0.1.1 - 2026-08-21

### Added
- History, environments and variables, per-request settings, and the command palette.

## 0.1.0 - 2026-08-20

The first release: tabs, the request editor, the HTTP engine, the JSON response viewer,
collections and sessions.

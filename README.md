# Intro

Hello! The purpose of this document is to give you some context about how this API works.

It works like this:

HTTP/WebSocket requests are handled by a request catcher, and are managed automatically by
the operating system's thread scheduler.

Your repository will be cloned to a local folder (only the git metadata) and a plethora of git log/show commands will retrieve metadata, parsing the command line outputs.

The parsed information is logically coalated into types and is send back to you!

# How it works

## Calling the API from your application

You can call the API via HTTP or using Websockets to recieve a reactive message!
This gives you flexibility as to whether:

- **http**: you don't want to manage any progress loading the repo and just want the data
- **ws**: you want to display any loading progress that might give the user something to mull about while the data loads in the background

Both methods require you to send a payload of some sorts. This payload should be in standard format, where git can clone the url safely from the command line taking it as an argument:

```
https://github.com/<OWNER>/<REPOSITORY_NAME>
```

Only HTTPS repository URLs on `GIT_ALLOWED_HOSTS` are accepted. URLs containing
credentials, custom ports, query strings, or fragments are rejected. Do not put
private-repository credentials in a URL: URLs can otherwise leak through errors,
proxies, Git remotes, and logs.

The optional `auth_token` request field supplies a per-request HTTPS Git token
for a private repository. Use a short-lived, read-only token with access only to
the requested repository. The service passes it to Git through a temporary
credential-helper environment and does not store it in the remote URL or command
arguments. Authenticated checkouts and in-flight request batches are isolated by
a SHA-256 token fingerprint, so a request without the token cannot join or reuse
an authenticated request for the same repository. The raw token is never used as
a directory name.

All analysis requests must also include `X-Git-Api-Key` with the configured
`GIT_API_TOKEN`. `GET /health` is the only unauthenticated application endpoint.

## Running the service

The service is a Rust binary. Git must be installed.

```
cargo run --release
```

It listens on `0.0.0.0:8081` (and, on Unix, on the Unix socket
`/tmp/haskell-ipc.sock`). A container image is built from `Dockerfile`; it runs as
a non-root user, includes Git and CA certificates, and exposes a health probe on
`GET /health`.

Configuration is read from environment variables:

| Variable | Meaning |
| --- | --- |
| `GIT_API_TOKEN` | Required value of the `X-Git-Api-Key` header. When unset, requests are not authenticated. A set but empty value still requires an (empty) header. |
| `NODE_ENV` | With `production`, startup fails unless `GIT_API_TOKEN` is at least 32 bytes. |
| `GIT_ALLOWED_HOSTS` | Comma-separated repository hosts (default `github.com,gitlab.com,bitbucket.org`). An empty list fails startup. |

Checks used by CI: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`,
`cargo test --locked`, and `cargo build --locked --release`. See
[docs/architecture.md](docs/architecture.md) for the design and compatibility notes.

## Protocol summary

- **HTTP**: `POST` to any path with a JSON body of at most 8192 bytes
  (`{"url": "...", "auth_token": "..."}`; `auth_token` is optional). Success is
  `200` with `{"type":"value","data":<RepositoryData>}`. Errors are
  `{"type":"error","message":"..."}`: `401` unauthorized, `400` oversized or
  invalid JSON, `500` analysis failures (message prefixed with `err: `). Other
  methods with a valid key get `405`.
- **WebSocket**: upgrade on any path with the API key header, then send one text
  or binary message with the same JSON payload. The service streams
  `{"type":"text_update","data":"..."}` progress messages, then a
  `{"type":"value",...}` result or an `error` envelope, and closes with reason
  `Done` (`Bad input` for malformed requests).

# How our code interfaces with the API

# HTTP

We can interface with the api using code like this:

```
fetch("http://" + API_CONN_ENDPOINT, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Git-Api-Key": GIT_API_TOKEN,
        },
        body: JSON.stringify({ url, auth_token }),
    }).then((response) => {
        if (!response.ok) reject(`API returned status ${response.status}`);
        response.json().then((d) => resolve(d.data));
    })
```

Things to note:
the method should always be POST, otherwise the API will reject the request

# WebSockets

The WebSocket upgrade request must include `X-Git-Api-Key`. Browser WebSocket
APIs cannot set arbitrary upgrade headers, so browser code must connect through
an authenticated server-side proxy. Never expose `GIT_API_TOKEN` to a browser.

Websockets can get a little complicated, so bear with me.
For every new repository you want analytics on, you must create a new Socket each time:

```
const socket = new WebSocket("ws://" + API_CONN_ENDPOINT)
```

This ensures that reactive messages from other repositories do not get tangled, and requests are handled efficiently you can setup the socket like this:

```
socket.onopen = () => {

    // notify that connection to the api was successful
    if (notifier !== null) notifier.next("Connected to the API!");

    // send data through socket
    socket.send(
        JSON.stringify({
          url,
          auth_token,
        })
    );
};
```

When the socket opens, you can safely send the payload containing the url you want to your analytics on.
The socket can respond like this:

```
socket.onmessage = (event: WebSocket.MessageEvent) => {
    try {

        const { data } = event;
        const parsed = JSON.parse(data);

        if (parsed.type === "text_update" && notifier !== null) notifier.next(parsed.data);

        else if (parsed.type === "error") {
          reject(parsed.message);
          socket.close();

        } else if (parsed.type === "value") {
          resolve(parsed.data);
          socket.close();
        }

    } catch (err) {
        reject(err);
        socket.close();
    }
};
```

You can see that the kind of data you are fetching can be three kinds:

- **parsed.type === "text_update"**: the data is a string which contains the most recent loading stages of your repository
- **parsed.type === "error"**: something went wrong inside the application
- **parsed.type === "value"**: the API has sent you your analytics! You can safely resolve the promise and parse away!

Make sure that you are also guarding for errors and are closing the socket after it is used up:

```
socket.onerror = (\_err: WebSocket.ErrorEvent) => {

    const s = "Encountered a Websocket Error";
    if (notifier !== null) notifier.next(s);
    reject(new Error(s));
    socket.close();
};
```

# Types

Using typescript as an example, you can use these types directly in your application:

**PS**: Keep in mind that _Map objects_ are automatically converted to plain objects in javascript/typescript so you can use functions to transpose those types to Map objects for convenience. They will work by having key: value entries where the key will be a parameter inside the object

This is the type that will be sent to you:

```
type RepositoryData = Readonly<{
    repoName: string;
    branches: BranchData[];
    allCommits: Map<string, CommitData>;
    contributors: Map<string, ContributorData>;
}>;

type BranchData = Readonly<{
    branchName: string;
    commitHashes: string[];
}>;

type CommitData = Readonly<{
    commitHash: string;
    commitTitle: string;
    contributorName: string;
    description: string;
    timestamp: string;
    fileData: FileChanges[];
}>;

type FileChanges = Readonly<{
    filepath: string;
    oldFilePath: string;
    char: ChangeType;
    likeness: number;
    newLines: number;
    deletedLines: number;
    diff: string[];
}>;

type ChangeType = "A" | "M" | "D" | "R" | "C";

type ContributorData = Readonly<{
    name: string;
    emails: string[];
}>;
```

# Final words

For any further queries on how to call from the API, leave comments on our github page and we will do our best to respond in a timely fashion regarding your requests/inquiries.
We thank you as a team for reading to the end of this document!

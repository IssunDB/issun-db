# Integrations

IssunDB provides integration servers to expose graph operations, vector search, and Cypher query execution to external applications and client tools. This document describes how to configure and run these services.

---

## HTTP REST API

The `issundb-rest` crate provides an HTTP REST server built on Axum. It serves versioned endpoints for node/edge CRUD operations, text and vector searches, and query execution.

### Start the REST Server

Launch the REST server via `cargo` using the following command:

```bash
cargo run -p issundb-rest -- --db-path /path/to/db-dir [--host 127.0.0.1] [--port 7474]
```

The server also accepts `--map-size-gb` to set the LMDB map size (default 4). Each flag falls back to an environment variable when omitted: `ISSUNDB_DB_PATH` for the database path, `ISSUNDB_REST_HOST` for the listen address (default `127.0.0.1`), and `ISSUNDB_REST_PORT` for the port (default 7474). The server binds without TLS or authentication by design; run it behind a reverse proxy that terminates TLS and enforces access control.

### Endpoint Reference

All data and query endpoints are prefixed with `/v1`.

#### Node Operations

* Create node: `POST /v1/nodes`
    * Request body:
      ```json
      {
        "label": "Person",
        "props": { "name": "Alice", "age": 30 }
      }
      ```
    * Response: Returns the generated `NodeId` wrapped in a JSON object, e.g., `{"id": 1}`.
* Get node: `GET /v1/nodes/:id`
    * Response: A JSON object containing the node's unique ID, labels, and properties.
* Update node: `PUT /v1/nodes/:id`
    * Request body:
      ```json
      {
        "props": { "name": "Bob", "age": 32 }
      }
      ```
* Delete node: `DELETE /v1/nodes/:id`
    * Response: `204 No Content` on successful removal.
* Add label: `POST /v1/nodes/:id/labels/:label`
    * Response: `204 No Content`; returns `404 Not Found` when the node does not exist.
* Remove label: `DELETE /v1/nodes/:id/labels/:label`
    * Response: `204 No Content` (label removal is idempotent).

#### Edge Operations

* Create edge: `POST /v1/edges`
    * Request body:
      ```json
      {
        "src": 1,
        "dst": 2,
        "type": "KNOWS",
        "props": { "since": 2020 }
      }
      ```
    * Response: Returns the generated `EdgeId` wrapped in a JSON object, e.g., `{"id": 1}`.
* Get edge: `GET /v1/edges/:id`
    * Response: A JSON object containing the edge's unique ID, source/destination node IDs, type, and properties.
* Update edge: `PUT /v1/edges/:id`
    * Request body:
      ```json
      {
        "props": { "since": 2021 }
      }
      ```
    * Response: `204 No Content`; returns `404 Not Found` when the edge does not exist.
* Delete edge: `DELETE /v1/edges/:id`
    * Response: `204 No Content` upon successful removal.

#### Search and Query Operations

* Cypher query: `POST /v1/query`
    * Request body:
      ```json
      {
        "query": "MATCH (n:Person) WHERE n.age > $min_age RETURN n.name",
        "params": { "min_age": 25 }
      }
      ```
    * Response: Returns a results table containing the records and projected column names.
* Explain plan: `POST /v1/explain`
    * Request body:
      ```json
      {
        "query": "MATCH (a)-[:KNOWS]->(b) RETURN a, b"
      }
      ```
    * Response: An indented, human-readable execution plan tree.
* Full-text search: `POST /v1/search/text`
    * Request body:
      ```json
      {
        "query": "search term",
        "label": "Document",
        "property": "content",
        "limit": 10
      }
      ```
* Vector search: `POST /v1/search/vector`
    * Request body:
      ```json
      {
        "vector": [0.1, 0.9, 0.4],
        "k": 5,
        "label": "Document"
      }
      ```

#### Vector and Retrieval Operations

* Upsert vector: `POST /v1/vectors`
    * Request body:
      ```json
      {
        "id": 1,
        "vector": [0.1, 0.9, 0.4]
      }
      ```
    * Response: Returns the node ID wrapped in a JSON object; an empty vector returns `400 Bad Request`.
* Delete vector: `DELETE /v1/vectors/:id`
    * Response: `204 No Content`; removes the embedding from the index and storage.
* Hybrid retrieval: `POST /v1/retrieve`
    * Request body (all fields are optional; provide a vector, a text query, or both to produce seed nodes):
      ```json
      {
        "vector": [0.1, 0.9, 0.4],
        "text_query": "transactional storage",
        "vector_k": 5,
        "text_k": 5,
        "text_label": "Document",
        "text_property": "content",
        "vector_label": null,
        "hops": 2,
        "max_distance": null,
        "max_nodes": null,
        "fusion_strategy": "rrf",
        "rrf_k": 60,
        "vector_weight": 0.5,
        "text_weight": 0.5
      }
      ```
    * Response: The induced subgraph as `nodes`, `edges`, and per-node `scores`. Defaults mirror the Rust `HybridRetrieveOptions` (`vector_k` 10, `text_k` 10, `hops` 2, and RRF fusion); `fusion_strategy` accepts `"rrf"` or `"weighted_sum"`, and an unknown value returns `400 Bad Request`.

#### Health Probe

* Health: `GET /health`
    * Unversioned so infrastructure probes do not track the API version; the body reports the crate `version` and the current `api` version.

#### API Reference (OpenAPI)

The server automatically publishes a machine-readable OpenAPI 3.1 document generated from the route handlers to match the live API. This document can be used to generate typed clients or browse request and response schemas.

* OpenAPI document: `GET /v1/openapi.json`
* Interactive Scalar UI: `GET /v1/docs`

The Scalar UI loads its front-end assets from a CDN, meaning the documentation page needs outbound network access to render; the `GET /v1/openapi.json` document itself is fully self-contained and works offline.

---

## Model Context Protocol (MCP) Server

The `issundb-mcp` crate implements a Model Context Protocol (MCP) server. It exposes database actions, search features, and query execution as standard MCP tools for LLM clients (such as Cursor, Claude Desktop, or custom agent frameworks).

### Start the MCP Server

The server supports two transport protocols:

#### Stdio Transport (Default)

This is standard for local client integrations where the LLM application launches the server as a background subprocess.

```bash
cargo run -p issundb-mcp -- --db-path /path/to/db-dir --transport stdio
```

#### Streamable HTTP Transport

For remote connections, serve over streamable HTTP:

```bash
cargo run -p issundb-mcp -- --db-path /path/to/db-dir --transport http --bind 127.0.0.1:8000
```

The endpoint is mounted at the path given by `--http-path` (default `/mcp`). Like the REST server, the process accepts `--map-size-gb` (default 4), and the flags fall back to environment variables when omitted: `ISSUNDB_DB_PATH`, `ISSUNDB_MCP_TRANSPORT`, and `ISSUNDB_MCP_BIND`.

The HTTP transport validates the `Host` header to block DNS rebinding attacks. The loopback names (`localhost`, `127.0.0.1`, and `::1`) and the `--bind` host are always accepted; a request with a missing or unknown `Host` receives `403 Forbidden`. When the server sits behind a reverse proxy, repeat `--allowed-host` for each public hostname the proxy forwards:

```bash
cargo run -p issundb-mcp -- --transport http --bind 0.0.0.0:8000 \
    --allowed-host mcp.example.com --allowed-host issundb.internal
```

TLS and authentication are the reverse proxy's job; the server itself binds without either by design.

### Exposed MCP Tools

The server registers the following tools with the connecting client:

1. `get_node`: Fetch a node by ID, returning its labels and properties.
2. `get_edge`: Fetch an edge by ID, returning its endpoints, type, and properties.
3. `cypher_query`: Execute a Cypher query with optional parameter bindings. `CREATE`, `SET`, `REMOVE`, `DELETE`, and `MERGE` statements can be used to mutate the graph.
4. `explain`: Return the physical query plan for a Cypher query as an indented tree.
5. `text_search`: Full-text search over indexed node properties; returns ranked hits.
6. `vector_search`: Nearest-neighbor vector search; returns the k closest nodes by distance (supporting label and property filtering).
7. `retrieve_hybrid`: Run a hybrid retrieval query that combines vector/semantic search, full-text keyword search, and relationship expansion.

### Client Configurations

To connect an LLM client to the IssunDB MCP server, use the following configurations:

#### Streamable HTTP

```json
{
    "mcpServers": {
        "issundb": {
            "url": "http://issundb-mcp-server-host:8000/mcp/"
        }
    }
}
```

Note that `issundb-mcp-server-host:8000` must be replaced with the actual host (or IP) and port of the MCP server.

#### Stdio

```json
{
    "mcpServers": {
        "issundb": {
            "command": "/absolute/path/to/issun-db/target/release/issundb-mcp",
            "args": [
                "--db-path",
                "/absolute/path/to/db-dir",
                "--transport",
                "stdio"
            ]
        }
    }
}
```

---

## Docker

The repository ships a `Dockerfile` that builds one image containing the `issundb-cli`, `issundb-rest`, and `issundb-mcp` binaries. Build it from the repository root with the GraphBLAS submodule checked out:

```bash
git submodule update --init external/GraphBLAS
docker build -t issundb .
```

The image stores the database at `/data` (declared as a volume) and sets `ISSUNDB_DB_PATH=/data`, so no `--db-path` argument is needed. The server defaults are adjusted for container use: the REST server binds `0.0.0.0:7474`, and the MCP server defaults to the Streamable HTTP transport on `0.0.0.0:8000`. The default command is the interactive CLI:

```bash
# Interactive CLI against a named volume
docker run --rm -it -v issundb-data:/data issundb

# REST server
docker run --rm -p 7474:7474 -v issundb-data:/data issundb issundb-rest

# MCP server over Streamable HTTP
docker run --rm -p 8000:8000 -v issundb-data:/data issundb issundb-mcp
```

The container network is the isolation boundary for the servers; TLS and authentication remain the reverse proxy's job.

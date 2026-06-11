# Integrations

IssunDB provides integration servers to expose graph operations, vector search, and Cypher execution to external applications and client tools.

---

## HTTP REST API

The `issundb-rest` crate provides an HTTP REST server built on Axum. It serves versioned endpoints for node/edge CRUD, text and vector searches,
and query execution.

### Start the REST Server

Run the REST server using `cargo`:

```bash
cargo run -p issundb-rest -- --db-path /path/to/db-dir [--host 127.0.0.1] [--port 7474]
```

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
    * Response: Returns a results table containing records and projected column names.
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

#### API Reference (OpenAPI)

The server publishes a machine-readable OpenAPI 3.1 document generated from the route handlers, so it always matches the live API. Use it to
generate typed clients or to browse the full request and response schemas, including the routes not enumerated above (`POST /v1/vectors` and
`POST /v1/retrieve`).

* OpenAPI document: `GET /v1/openapi.json`
* Interactive Scalar UI: `GET /v1/docs`

The Scalar UI loads its front-end assets from a CDN, so the documentation page needs outbound network access to render; the
`GET /v1/openapi.json` document itself is fully self-contained and works offline.

---

## Model Context Protocol (MCP) Server

The `issundb-mcp` crate implements a Model Context Protocol (MCP) server. It exposes database actions, search features, and query execution as
standard MCP tools that LLM clients (such as Cursor, Claude Desktop, or custom agent frameworks) can invoke.

### Start the MCP Server

The server supports two transport protocols:

#### Stdio Transport (Default)

Standard for local client integrations where the LLM application launches the server as a background subprocess.

```bash
cargo run -p issundb-mcp -- --db-path /path/to/db-dir --transport stdio
```

#### Streamable HTTP Transport

For remote connections, using streamable HTTP.

```bash
cargo run -p issundb-mcp -- --db-path /path/to/db-dir --transport http --bind 127.0.0.1:8000
```

### Exposed MCP Tools

The server registers the following tools with the connecting client:

1. `get_node`: Fetch a node by ID; returning its labels and properties.
2. `get_edge`: Fetch an edge by ID; returning its endpoints, type, and properties.
3. `cypher_query`: Execute a Cypher query with optional parameter bindings. Use CREATE, SET, REMOVE, DELETE, and MERGE to mutate the graph.
4. `explain`: Return the physical query plan for a Cypher query as an indented tree.
5. `text_search`: Full-text search over indexed node properties; returns ranked hits.
6. `vector_search`: Nearest-neighbor vector search; returns the $k$ closest nodes by distance (with optional label and property filtering).
7. `retrieve_hybrid`: Execute a hybrid retrieval query that combines vector/semantic search, full-text keyword search, and relationship expansion.

### Client Configurations

To connect an LLM client (like Claude Code) to the IssunDB MCP server, you can use the following configurations:

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

Note that `issundb-mcp-server-host:8000` must be the actual host (or ip) and port of the MCP server.

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

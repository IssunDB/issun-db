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

### Endpoint Reference

All data and query endpoints are prefixed with `/v1`.

#### Node Operations

* **Create Node**: `POST /v1/nodes`
    * Request body:
      ```json
      {
        "label": "Person",
        "props": { "name": "Alice", "age": 30 }
      }
      ```
    * Response: Returns the generated `NodeId` wrapped in a JSON object, e.g., `{"id": 1}`.
* **Get Node**: `GET /v1/nodes/:id`
    * Response: A JSON object containing the node's unique ID, labels, and properties.
* **Update Node**: `PUT /v1/nodes/:id`
    * Request body:
      ```json
      {
        "props": { "name": "Bob", "age": 32 }
      }
      ```
* **Delete Node**: `DELETE /v1/nodes/:id`
    * Response: `204 No Content` on successful removal.
* **Add Label**: `POST /v1/nodes/:id/labels/:label`
    * Response: `204 No Content`; returns `404 Not Found` when the node does not exist.
* **Remove Label**: `DELETE /v1/nodes/:id/labels/:label`
    * Response: `204 No Content` (label removal is idempotent).

#### Edge Operations

* **Create Edge**: `POST /v1/edges`
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
* **Get Edge**: `GET /v1/edges/:id`
    * Response: A JSON object containing the edge's unique ID, source/destination node IDs, type, and properties.
* **Update Edge**: `PUT /v1/edges/:id`
    * Request body:
      ```json
      {
        "props": { "since": 2021 }
      }
      ```
    * Response: `204 No Content`; returns `404 Not Found` when the edge does not exist.
* **Delete Edge**: `DELETE /v1/edges/:id`
    * Response: `204 No Content` upon successful removal.

#### Search and Query Operations

* **Cypher Query**: `POST /v1/query`
    * Request body:
      ```json
      {
        "query": "MATCH (n:Person) WHERE n.age > $min_age RETURN n.name",
        "params": { "min_age": 25 }
      }
      ```
    * Response: Returns a results table containing the records and projected column names.
* **Explain Plan**: `POST /v1/explain`
    * Request body:
      ```json
      {
        "query": "MATCH (a)-[:KNOWS]->(b) RETURN a, b"
      }
      ```
    * Response: An indented, human-readable execution plan tree.
* **Full-Text Search**: `POST /v1/search/text`
    * Request body:
      ```json
      {
        "query": "search term",
        "label": "Document",
        "property": "content",
        "limit": 10
      }
      ```
* **Vector Search**: `POST /v1/search/vector`
    * Request body:
      ```json
      {
        "vector": [0.1, 0.9, 0.4],
        "k": 5,
        "label": "Document"
      }
      ```

#### API Reference (OpenAPI)

The server automatically publishes a machine-readable OpenAPI 3.1 document generated from the route handlers to match the live API. This document can be used to generate typed clients or browse request and response schemas, including routes such as `POST /v1/vectors`, `DELETE /v1/vectors/:id`, and `POST /v1/retrieve`.

* **OpenAPI Document**: `GET /v1/openapi.json`
* **Interactive Scalar UI**: `GET /v1/docs`

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

For remote connections, we can serve over streamable HTTP:

```bash
cargo run -p issundb-mcp -- --db-path /path/to/db-dir --transport http --bind 127.0.0.1:8000
```

### Exposed MCP Tools

The server registers the following tools with the connecting client:

1. `get_node`: Fetch a node by ID, returning its labels and properties.
2. `get_edge`: Fetch an edge by ID, returning its endpoints, type, and properties.
3. `cypher_query`: Execute a Cypher query with optional parameter bindings. `CREATE`, `SET`, `REMOVE`, `DELETE`, and `MERGE` statements can be used to mutate the graph.
4. `explain`: Return the physical query plan for a Cypher query as an indented tree.
5. `text_search`: Full-text search over indexed node properties; returns ranked hits.
6. `vector_search`: Nearest-neighbor vector search; returns the $k$ closest nodes by distance (supporting label and property filtering).
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

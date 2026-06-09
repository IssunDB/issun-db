# Integrations

IssunDB provides integration servers to expose graph operations, vector search, and Cypher execution to external applications and client tools.

---

## HTTP REST API

The `issundb-rest` crate provides an HTTP REST server built on Axum. It serves versioned endpoints for node/edge CRUD, text/vector searches, and query execution.

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
  * Response: Returns the generated `NodeId` as a JSON number.
* Get node: `GET /v1/nodes/:id`
  * Response: A JSON object containing the node's unique ID, labels, and properties.
* Update node: `PUT /v1/nodes/:id`
  * Request body: JSON properties to replace the existing property map.
* Delete node: `DELETE /v1/nodes/:id`
  * Response: `200 OK` upon successful removal.

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
  * Response: Returns the generated `EdgeId` as a JSON number.
* Get edge: `GET /v1/edges/:id`
  * Response: A JSON object containing the edge's unique ID, source/destination node IDs, type, and properties.
* Delete edge: `DELETE /v1/edges/:id`

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

The `issundb-mcp` crate implements a Model Context Protocol (MCP) server. It exposes database actions, search features, and query execution as standard MCP tools that LLM clients (such as Cursor, Claude Desktop, or custom agent frameworks) can invoke.

### Start the MCP Server

The server supports two transport protocols:

#### Stdio Transport (Default)
Standard for local client integrations where the LLM application launches the server as a background subprocess.

```bash
cargo run -p issundb-mcp -- --db-path /path/to/db-dir --transport stdio
```

#### HTTP SSE Transport
For remote connections, using Server-Sent Events (SSE).

```bash
cargo run -p issundb-mcp -- --db-path /path/to/db-dir --transport http --bind 127.0.0.1:8000
```

### Exposed MCP Tools

The server registers the following tools with the connecting client:

1. `add_node`: Creates a node with a label and properties.
2. `get_node`: Retrieves a node's details by ID.
3. `update_node`: Updates a node's property map.
4. `delete_node`: Deletes a node and its attached edges.
5. `add_edge`: Creates a directed relationship between two nodes.
6. `get_edge`: Retrieves an edge's details by ID.
7. `delete_edge`: Removes an edge by ID.
8. `query`: Executes a Cypher query with optional parameter bindings.
9. `explain`: Evaluates and prints a Cypher query's physical plan.
10. `search_text`: Queries the BM25 full-text search index.
11. `search_vector`: Performs a k-nearest-neighbor vector similarity search.

"""Type stubs for the IssunDB Python bindings.

These hints describe the compiled `IssunDB` class exported from the native
extension. Property maps and query results cross the boundary as JSON strings,
so callers serialize with `json.dumps` on the way in and `json.loads` on the way
out.
"""

from typing import List, Optional, Union


class IssunDB:
    """A handle to an IssunDB graph database.

    The handle owns the underlying LMDB environment for as long as it is alive.
    Writes are serialized internally, so a single handle is safe to share.
    """

    def __init__(self, path: str, map_size_gb: Optional[int] = None) -> None:
        """Open or create an IssunDB graph at ``path``.

        Args:
            path: Filesystem directory for the LMDB environment. It is created
                if it does not exist.
            map_size_gb: Optional map size in gigabytes (defaults to 1).

        Raises:
            RuntimeError: If the environment cannot be opened.
        """
        ...

    def add_node(self, labels: Union[str, List[str]], props: str) -> int:
        """Insert a node with ``labels`` and JSON-encoded ``props``.

        Args:
            labels: A single label string, or a list of label strings for a
                multi-label node.
            props: A JSON object string holding the node properties.

        Returns:
            The new node ID.

        Raises:
            ValueError: If ``props`` is not valid JSON.
            RuntimeError: If the write fails.
        """
        ...

    def get_node(self, id: int) -> Optional[str]:
        """Return the JSON-encoded properties of node ``id``.

        Args:
            id: The node ID.

        Returns:
            The JSON object string for the node, or ``None`` if no such node
            exists.

        Raises:
            RuntimeError: If the read or decode fails.
        """
        ...

    def update_node(self, id: int, props: str) -> None:
        """Replace the properties of node ``id`` with JSON-encoded ``props``.

        Args:
            id: The node ID.
            props: A JSON object string holding the replacement properties.

        Raises:
            ValueError: If ``props`` is not valid JSON.
            RuntimeError: If the node does not exist or the write fails.
        """
        ...

    def delete_node(self, id: int) -> None:
        """Delete node ``id`` and all of its incident edges.

        Args:
            id: The node ID.

        Raises:
            RuntimeError: If the write fails.
        """
        ...

    def add_label(self, id: int, label: str) -> None:
        """Add a label to node ``id``.

        Args:
            id: The node ID.
            label: The label to add.

        Raises:
            RuntimeError: If the node does not exist or the write fails.
        """
        ...

    def remove_label(self, id: int, label: str) -> None:
        """Remove a label from node ``id``. No-op when the node or label is missing.

        Args:
            id: The node ID.
            label: The label to remove.

        Raises:
            RuntimeError: If the write fails.
        """
        ...

    def add_edge(self, src: int, dst: int, etype: str, props: str) -> int:
        """Insert a directed edge from ``src`` to ``dst``.

        Args:
            src: The source node ID.
            dst: The destination node ID.
            etype: The edge type.
            props: A JSON object string holding the edge properties.

        Returns:
            The new edge ID.

        Raises:
            ValueError: If ``props`` is not valid JSON.
            RuntimeError: If the write fails.
        """
        ...

    def get_edge(self, id: int) -> Optional[str]:
        """Return edge ``id`` as a JSON string.

        Args:
            id: The edge ID.

        Returns:
            A JSON object string of the shape
            ``{"src": int, "dst": int, "type": str, "props": {...}}``, or
            ``None`` if no such edge exists.

        Raises:
            RuntimeError: If the read or decode fails.
        """
        ...

    def update_edge(self, id: int, props: str) -> None:
        """Replace the properties of edge ``id`` with JSON-encoded ``props``.

        Args:
            id: The edge ID.
            props: A JSON object string holding the replacement properties.

        Raises:
            ValueError: If ``props`` is not valid JSON.
            RuntimeError: If the edge does not exist or the write fails.
        """
        ...

    def delete_edge(self, id: int) -> None:
        """Delete edge ``id``.

        Args:
            id: The edge ID.

        Raises:
            RuntimeError: If the write fails.
        """
        ...

    def query(self, cypher: str, params: Optional[str] = None) -> str:
        """Execute a Cypher query and return the result as a JSON string.

        Args:
            cypher: The Cypher query text.
            params: An optional JSON object string holding parameter bindings.

        Returns:
            A JSON object string of the shape
            ``{"columns": [...], "records": [[...]]}``.

        Raises:
            ValueError: If ``params`` is not a JSON object string.
            RuntimeError: If the query fails to parse, plan, or execute.
        """
        ...

    def explain(self, cypher: str) -> str:
        """Compile and optimize ``cypher`` and return the plan as a tree.

        Args:
            cypher: The Cypher query text.

        Returns:
            A human-readable optimized physical plan.

        Raises:
            RuntimeError: If the query fails to parse or plan.
        """
        ...

    def upsert_vector(self, id: int, vec: List[float]) -> None:
        """Index or update the float32 embedding for node ``id``.

        Args:
            id: The node ID.
            vec: The embedding as a list of floats.

        Raises:
            RuntimeError: If the write fails.
        """
        ...

    def remove_vector(self, id: int) -> None:
        """Remove the indexed vector for node ``id``. No-op when absent.

        Args:
            id: The node ID.

        Raises:
            RuntimeError: If the write fails.
        """
        ...

    def vector_search(
        self,
        vec: List[float],
        k: int,
        label: Optional[str] = None,
        properties: Optional[str] = None,
        rescore_factor: Optional[int] = None,
    ) -> str:
        """Return the ``k`` nearest neighbors to ``vec``.

        Args:
            vec: The query embedding as a list of floats.
            k: The number of neighbors to return.
            label: Restricts search to nodes carrying this label.
            properties: A JSON object string of key-value property filters.
            rescore_factor: Optional candidate over-fetching multiplier.

        Returns:
            A JSON array string of ``{"node": int, "distance": float}`` objects.

        Raises:
            RuntimeError: If the search fails.
        """
        ...

    def configure_vector_index(
        self,
        metric: str,
        quantization: str = "float32",
        reindex: bool = False,
    ) -> None:
        """Configure or rebuild the vector index.

        Args:
            metric: One of 'cosine', 'l2', or 'dot'.
            quantization: One of 'float32', 'float16', or 'int8'.
            reindex: Rebuild the index from existing vectors under the new
                configuration.

        Raises:
            RuntimeError: If the configuration is invalid or, without
                ``reindex``, vectors already exist under a different
                configuration.
        """
        ...

    def text_search(
        self,
        query: str,
        label: Optional[str] = None,
        property: Optional[str] = None,
        limit: int = 10,
    ) -> str:
        """Full-text search over indexed node properties.

        Args:
            query: The search query text.
            label: Optional label narrowing the search to one index.
            property: Optional property narrowing the search to one index.
            limit: The maximum number of results to return.

        Returns:
            A JSON array string of ``{"node": int, "score": float}`` objects.

        Raises:
            RuntimeError: If the search fails.
        """
        ...

    def create_text_index(
        self,
        label: str,
        property: str,
        language: Optional[str] = None,
    ) -> None:
        """Create a full-text index on ``property`` for nodes with ``label``.

        Args:
            label: The node label to index.
            property: The property to index.
            language: Optional analyzer language (defaults to English).

        Raises:
            RuntimeError: If the index cannot be created.
        """
        ...

    def drop_text_index(self, label: str, property: str) -> None:
        """Drop the full-text index on ``property`` for nodes with ``label``.

        Args:
            label: The indexed node label.
            property: The indexed property.

        Raises:
            RuntimeError: If the index cannot be dropped.
        """
        ...

    def has_text_index(self, label: str, property: str) -> bool:
        """Check whether a full-text index exists on ``property`` for ``label``.

        Args:
            label: The node label.
            property: The property name.

        Returns:
            ``True`` if the index exists, ``False`` otherwise.

        Raises:
            RuntimeError: If the read fails.
        """
        ...

    def list_text_indexes(self) -> str:
        """List all full-text indexes.

        Returns:
            A JSON array string of
            ``{"label": str, "property": str, "language": str}`` objects.

        Raises:
            RuntimeError: If the read fails.
        """
        ...

    def retrieve_hybrid(
        self,
        vector: Optional[List[float]] = None,
        text_query: Optional[str] = None,
        vector_k: int = 10,
        text_k: int = 10,
        text_label: Optional[str] = None,
        text_property: Optional[str] = None,
        vector_label: Optional[str] = None,
        hops: int = 2,
        max_distance: Optional[float] = None,
        max_nodes: Optional[int] = None,
        fusion_strategy: str = "rrf",
        rrf_k: int = 60,
        vector_weight: float = 0.5,
        text_weight: float = 0.5,
    ) -> str:
        """Execute a hybrid retrieval combining vector, text, and graph expansion.

        Returns:
            A JSON object string of the shape
            ``{"nodes": [...], "edges": [...], "scores": {...}}``.

        Raises:
            RuntimeError: If the retrieval fails.
        """
        ...

    def set_thread_count(self, count: int) -> None:
        """Set the GraphBLAS thread count (0 restores the default).

        Args:
            count: The number of threads.

        Raises:
            RuntimeError: If the count cannot be applied.
        """
        ...

    def backup(self, path: str) -> None:
        """Write a hot backup of the database to ``path``.

        Args:
            path: The destination snapshot path.

        Raises:
            RuntimeError: If the backup fails.
        """
        ...

    def backup_compact(self, path: str) -> None:
        """Write a compacted hot backup of the database to ``path``.

        Args:
            path: The destination snapshot path.

        Raises:
            RuntimeError: If the backup fails.
        """
        ...

    @staticmethod
    def restore(snapshot: str, dst: str) -> None:
        """Restore a snapshot into a new database directory.

        Open the restored database with ``IssunDB(dst)`` afterward.

        Args:
            snapshot: The source snapshot path.
            dst: The destination database directory.

        Raises:
            RuntimeError: If the restore fails.
        """
        ...

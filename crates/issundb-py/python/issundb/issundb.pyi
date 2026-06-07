"""Type stubs for the IssunDB Python bindings.

These hints describe the compiled `IssunDB` class exported from the native
extension. Property maps and query results cross the boundary as JSON strings,
so callers serialize with `json.dumps` on the way in and `json.loads` on the way
out.
"""

from typing import List, Optional


class IssunDB:
    """A handle to an IssunDB graph database.

    The handle owns the underlying LMDB environment for as long as it is alive.
    Writes are serialized internally, so a single handle is safe to share.
    """

    def __init__(self, path: str) -> None:
        """Open or create an IssunDB graph at ``path``.

        Args:
            path: Filesystem directory for the LMDB environment. It is created
                if it does not exist.

        Raises:
            RuntimeError: If the environment cannot be opened.
        """
        ...

    def add_node(self, label: str, props: str) -> int:
        """Insert a node with ``label`` and JSON-encoded ``props``.

        Args:
            label: The node label.
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
            RuntimeError: If the write fails.
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

    def query(self, cypher: str) -> str:
        """Execute a Cypher query and return the result as a JSON string.

        Args:
            cypher: The Cypher query text.

        Returns:
            A JSON object string of the shape
            ``{"columns": [...], "records": [[...]]}``.

        Raises:
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

    def vector_search(self, vec: List[float], k: int) -> str:
        """Return the ``k`` nearest neighbors to ``vec``.

        Args:
            vec: The query embedding as a list of floats.
            k: The number of neighbors to return.

        Returns:
            A JSON array string of ``{"node": int, "distance": float}`` objects.

        Raises:
            RuntimeError: If the search fails.
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

    def create_text_index(self, label: str, property: str) -> None:
        """Create a full-text index on ``property`` for nodes with ``label``.

        Args:
            label: The node label to index.
            property: The property to index.

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

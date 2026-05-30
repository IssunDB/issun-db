# Current Limitations

This page outlines the currently known architectural and implementation constraints in IssunDB.

## Cypher Language Support

The query compiler parses and executes a specific, practical subset of the openCypher specification. The following features are currently under development or not yet supported:

- **Multiple Graph Operations**: Standard queries are confined to a single graph instance.
- **Complex Pattern Comprehensions**: List comprehensions that evaluate paths, such as `[(a)-->() | 1]`, are currently unsupported.
- **Advanced Query Procedures**: Procedural system calls, such as user-defined procedures or built-in system metadata procedures, are under development.

## Write Operations Serialization

Writes to the database are fully serialized. While read transactions execute concurrently without locking, all write operations require obtaining an exclusive global write lock on the `Graph` coordinator.

## Storage Capacity and Limits

The underlying database uses LMDB memory mapping:

- **Database Size Boundaries**: The maximum database size is constrained by the virtual memory limits of the host operating system and the `map_size_gb` parameter configured during initialization.
- **Adjacency Entries**: Extremely dense nodes with millions of outgoing or incoming relationships may experience increased memory footprints during complete CSR (Compressed Sparse Row) snapshot rebuilding.

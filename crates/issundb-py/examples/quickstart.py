import os
import shutil
import json
from issundb import IssunDB

def main():
    db_path = "./issundb-py-quickstart-data"

    # Clean up from previous run if any
    if os.path.exists(db_path):
        shutil.rmtree(db_path)

    print("IssunDB Python Quickstart Example")
    print("================================")

    # 1. Open the database
    db = IssunDB(db_path)
    print(f"Opened database at: {db_path}")

    # 2. Add some nodes
    alice_props = json.dumps({"name": "Alice", "age": 30})
    alice_id = db.add_node("Person", alice_props)
    print(f"Created node Alice with ID: {alice_id}")

    bob_props = json.dumps({"name": "Bob", "age": 25})
    bob_id = db.add_node("Person", bob_props)
    print(f"Created node Bob with ID: {bob_id}")

    # 3. Connect them with an edge
    edge_props = json.dumps({"since": 2021})
    edge_id = db.add_edge(alice_id, bob_id, "KNOWS", edge_props)
    print(f"Created KNOWS edge with ID: {edge_id}")

    # 4. Query using Cypher
    cypher_query = "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a.name, b.name, r.since"
    print(f"\nExecuting query: {cypher_query}")

    result_str = db.query(cypher_query)
    result = json.loads(result_str)

    print("\nQuery results:")
    print("Columns:", result["columns"])
    for record in result["records"]:
        values = record["values"]
        print(f"  - {values[0]} knows {values[1]} since {values[2]}")

    # 5. Explain query execution plan
    print("\nQuery Explanation Plan:")
    plan = db.explain(cypher_query)
    print(plan)

    # Clean up database files
    if os.path.exists(db_path):
        shutil.rmtree(db_path)
    print("Database cleaned up successfully.")

if __name__ == "__main__":
    main()

import os
import shutil
import json
from issundb import IssunDB

def main():
    db_path = "./issundb-py-search-data"
    
    # Clean up from previous run if any
    if os.path.exists(db_path):
        shutil.rmtree(db_path)

    print("IssunDB Python Vector & Text Search Example")
    print("==========================================")

    # 1. Open the database
    db = IssunDB(db_path)
    print(f"Opened database at: {db_path}")

    # 2. Create a full-text search index on Movie.description
    db.create_text_index("Movie", "description")
    print("Created full-text search index on Movie.description")

    # 3. Create movie nodes
    movies = [
        {"title": "Inception", "description": "A thief who steals corporate secrets through dream-sharing technology", "vec": [0.9, 0.1, 0.2, 0.0]},
        {"title": "The Matrix", "description": "A computer hacker learns about the true nature of his reality", "vec": [0.8, 0.2, 0.1, 0.1]},
        {"title": "Interstellar", "description": "A team of explorers travel through a wormhole in space near a black hole", "vec": [0.1, 0.9, 0.3, 0.2]},
        {"title": "Gravity", "description": "Two astronauts work together to survive after an accident in outer space", "vec": [0.2, 0.8, 0.4, 0.1]}
    ]

    movie_ids = []
    for movie in movies:
        props = json.dumps({"title": movie["title"], "description": movie["description"]})
        node_id = db.add_node("Movie", props)
        movie_ids.append((node_id, movie))
        print(f"Created Movie '{movie['title']}' with ID: {node_id}")

    # 4. Upsert vectors/embeddings for these movies
    for node_id, movie in movie_ids:
        db.upsert_vector(node_id, movie["vec"])
        print(f"Indexed 4D vector for movie '{movie['title']}' (ID: {node_id})")

    # 5. Perform Full-Text Search
    print("\n--- 1. Performing Full-Text Search ---")
    query_text = "space wormhole"
    print(f"Query: '{query_text}'")
    fts_results_str = db.text_search(query_text, label="Movie", property="description", limit=2)
    fts_results = json.loads(fts_results_str)
    
    for hit in fts_results:
        node_id = hit["node"]
        score = hit["score"]
        node_data = json.loads(db.get_node(node_id))
        print(f"  Match: ID={node_id}, Title='{node_data['title']}', Score={score:.4f}")

    # 6. Perform Vector Search
    print("\n--- 2. Performing Vector Search ---")
    # Query vector is close to Inception/Matrix
    query_vector = [0.85, 0.15, 0.18, 0.05]
    print(f"Query vector: {query_vector}")
    vec_results_str = db.vector_search(query_vector, k=2)
    vec_results = json.loads(vec_results_str)

    for hit in vec_results:
        node_id = hit["node"]
        distance = hit["distance"]
        node_data = json.loads(db.get_node(node_id))
        print(f"  Match: ID={node_id}, Title='{node_data['title']}', Distance={distance:.4f}")

    # Clean up database files
    if os.path.exists(db_path):
        shutil.rmtree(db_path)
    print("\nDatabase cleaned up successfully.")

if __name__ == "__main__":
    main()

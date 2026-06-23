import asyncio
import json
import logging
import sys
import os

# Ensure we can import the local graphrag_sdk package
sys.path.insert(0, os.path.abspath("tmp/GraphRAG-SDK/graphrag_sdk/src"))

from graphrag_sdk import GraphRAG, LiteLLM, LiteLLMEmbedder
from graphrag_sdk.core.issundb_connection import IssunDBConnection

logging.basicConfig(level=logging.INFO, format="%(asctime)s - %(name)s - %(levelname)s - %(message)s")

async def main():
    # Verify api key
    api_key = os.environ.get("OPENROUTER_API_KEY")
    if not api_key:
        print("Error: OPENROUTER_API_KEY environment variable is not set.")
        sys.exit(1)

    # 1. Initialize models via OpenRouter
    llm = LiteLLM(
        model="openrouter/google/gemini-2.5-flash",
        api_key=api_key
    )
    embedder = LiteLLMEmbedder(
        model="openrouter/openai/text-embedding-3-small",
        api_key=api_key,
        dimensions=256
    )

    # 2. Initialize IssunDB connection
    conn = IssunDBConnection("issundb_novel_bench")
    
    # Delete previous graph to start fresh
    print("Deleting previous benchmark graph if any...")
    await conn.delete_graph()

    # 3. Load the corpus and subset the first document
    print("Loading corpus...")
    corpus = json.load(open("tmp/GraphRAG-Benchmark/Datasets/Corpus/novel.json"))
    doc = corpus[0]
    
    # We subset the first 100,000 characters to keep it fast, efficient, and cost-effective
    doc_text = doc["context"][:100000]
    print(f"Ingesting {doc['corpus_name']} (length: {len(doc_text)} chars)...")

    # 4. Ingest using GraphRAG
    async with GraphRAG(
        connection=conn,
        llm=llm,
        embedder=embedder,
        embedding_dimension=256
    ) as rag:
        result = await rag.ingest(
            text=doc_text,
            document_id=doc["corpus_name"]
        )
        print(f"Ingestion complete: {result.nodes_created} nodes, {result.relationships_created} edges created.")

        # Finalize (run deduplication + indexing)
        print("Finalizing graph...")
        await rag.finalize()
        print("Graph finalized successfully!")

        # 5. Query and evaluate
        questions = [
            "According to the narrative in 'Vestiges of the Mayas,' where is the territory known as Tierra de Guerra located?",
            "Which archaeological site is noted for its Maya inscriptions in the text?",
            "According to the narrative, with which distant region did the inhabitants of Mayab have early communications?"
        ]
        
        for q in questions:
            print("\n" + "="*80)
            print(f"Question: {q}")
            print("="*80)
            ans = await rag.completion(q)
            print(f"Answer: {ans.answer}")

if __name__ == "__main__":
    asyncio.run(main())

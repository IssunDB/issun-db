use issundb::{Graph, GraphQueryExt, Language, TextIndexExt};
use serde_json::json;
use tempfile::TempDir;

#[test]
fn export_rejects_metadata_collisions_before_overwriting_files() {
    for format in ["jsonl", "csv", "parquet"] {
        for key in [
            "_id", "_labels", "_label", "_from", "_to", "_type", "_etype",
        ] {
            let dir = TempDir::new().unwrap();
            let graph = Graph::open(&dir.path().join("db"), 1).unwrap();
            let props = json!({(key): 0});
            if matches!(key, "_id" | "_labels" | "_label") {
                graph.add_node("N", &props).unwrap();
            } else {
                let a = graph.add_node("N", &json!({})).unwrap();
                let b = graph.add_node("N", &json!({})).unwrap();
                graph.add_edge(a, b, "E", &props).unwrap();
            }
            let export = dir.path().join("export");
            std::fs::create_dir(&export).unwrap();
            let file = export.join(format!("nodes.{format}"));
            std::fs::write(&file, "previous export").unwrap();
            let error = graph
                .query(&format!(
                    "EXPORT DATABASE '{}' WITH {{format: '{format}'}}",
                    export.display(),
                ))
                .unwrap_err();
            assert!(error.to_string().contains("reserved property"), "{error}");
            assert_eq!(std::fs::read_to_string(file).unwrap(), "previous export");
        }
    }
}

#[test]
fn export_import_preserves_property_types_and_csv_special_characters() {
    for format in ["jsonl", "csv", "parquet"] {
        let dir = TempDir::new().unwrap();
        let graph = Graph::open(&dir.path().join("source"), 1).unwrap();
        let props = json!({
            "list_string": "[1,2]", "map_string": "{\"a\":1}",
            "text": "hello\nworld\r\n\"quoted\", text", "empty": "",
            "number_string": "123", "bool_string": "true",
            "list": [1, 2], "map": {"a": 1}, "null": null,
            "bool": true, "number": 123, "comma,\"newline\nkey": "日本語",
        });
        let node = graph.add_node("Odd:Label;Other", &props).unwrap();
        graph.add_edge(node, node, "[1,2]", &props).unwrap();
        let export = dir.path().join("export");
        graph
            .query(&format!(
                "EXPORT DATABASE '{}' WITH {{format: '{format}'}}",
                export.display(),
            ))
            .unwrap();
        let restored = Graph::open(&dir.path().join("restored"), 1).unwrap();
        restored
            .query(&format!("IMPORT DATABASE '{}'", export.display()))
            .unwrap();
        let ids = restored.all_nodes().unwrap();
        assert_eq!(ids.len(), 1, "{format}");
        assert_eq!(
            restored.node_labels(ids[0]).unwrap(),
            vec!["Odd:Label;Other"]
        );
        let edges = restored.out_neighbors(ids[0]).unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(
            restored.type_name(edges[0].edge_type).unwrap().as_deref(),
            Some("[1,2]")
        );
        for (key, value) in props.as_object().unwrap() {
            assert_eq!(
                restored.node_prop_json(ids[0], key).unwrap(),
                Some(value.clone()),
                "{format}: {key}"
            );
            assert_eq!(
                restored.edge_prop_json(edges[0].edge, key).unwrap(),
                Some(value.clone()),
                "{format}: {key}"
            );
        }
    }
}

#[test]
fn copy_csv_reads_multiline_records_and_rolls_back_unterminated_fields() {
    let dir = TempDir::new().unwrap();
    let graph = Graph::open(&dir.path().join("db"), 1).unwrap();
    let path = dir.path().join("nodes.csv");
    std::fs::write(
        &path,
        "\"body,\ntext\"\r\n\"hello\nworld\r\n\"\"quoted\"\"\"\r\n",
    )
    .unwrap();
    graph
        .query(&format!("COPY Doc FROM '{}'", path.display()))
        .unwrap();
    let ids = graph.all_nodes().unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(
        graph.node_prop_json(ids[0], "body,\ntext").unwrap(),
        Some(json!("hello\nworld\r\n\"quoted\""))
    );

    for invalid in ["body\nok\n\"unterminated", "body\nok\n\"closed\"extra\n"] {
        std::fs::write(&path, invalid).unwrap();
        assert!(
            graph
                .query(&format!("COPY Doc FROM '{}'", path.display()))
                .is_err()
        );
        assert_eq!(graph.all_nodes().unwrap(), ids);
    }
}

#[test]
fn export_import_preserves_escaped_schema_identifiers() {
    let dir = TempDir::new().unwrap();
    let graph = Graph::open(&dir.path().join("source"), 1).unwrap();
    let label = "Odd `; Label\nx";
    let property = "odd `; prop\nx";
    let etype = "Odd `; Type\nx";
    let node = graph
        .add_node(label, &json!({(property): "unique", "required": 1}))
        .unwrap();
    graph
        .add_edge(
            node,
            node,
            etype,
            &json!({(property): "unique", "required": 1, "index": 2}),
        )
        .unwrap();
    graph
        .create_node_unique_constraint(label, property)
        .unwrap();
    graph
        .create_node_required_constraint(label, "required")
        .unwrap();
    graph
        .create_edge_unique_constraint(etype, property)
        .unwrap();
    graph
        .create_edge_required_constraint(etype, "required")
        .unwrap();
    graph.create_edge_property_index(etype, "index").unwrap();
    graph.create_text_index(label, property).unwrap();
    let export = dir.path().join("export");
    graph
        .query(&format!("EXPORT DATABASE '{}'", export.display()))
        .unwrap();
    std::fs::remove_file(export.join("text_indexes.json")).unwrap();
    let restored = Graph::open(&dir.path().join("restored"), 1).unwrap();
    restored
        .query(&format!("IMPORT DATABASE '{}'", export.display()))
        .unwrap();
    assert_eq!(
        graph.list_node_indexes_and_constraints().unwrap(),
        restored.list_node_indexes_and_constraints().unwrap()
    );
    assert_eq!(
        graph.list_edge_indexes_and_constraints().unwrap(),
        restored.list_edge_indexes_and_constraints().unwrap()
    );
    assert_eq!(
        graph.list_text_indexes().unwrap(),
        restored.list_text_indexes().unwrap()
    );
    assert!(
        restored
            .add_node(label, &json!({(property): "unique", "required": 1}))
            .is_err()
    );
    assert!(
        restored
            .add_node(label, &json!({(property): "other"}))
            .is_err()
    );
}

#[test]
fn export_import_preserves_text_index_languages_and_legacy_exports() {
    let dir = TempDir::new().unwrap();
    let graph = Graph::open(&dir.path().join("source"), 1).unwrap();
    graph.add_node("Doc", &json!({"body": "Häuser"})).unwrap();
    graph
        .create_text_index_with_language("Doc", "body", Language::German)
        .unwrap();
    let export = dir.path().join("export");
    graph
        .query(&format!("EXPORT DATABASE '{}'", export.display()))
        .unwrap();
    let restored = Graph::open(&dir.path().join("restored"), 1).unwrap();
    restored
        .query(&format!("IMPORT DATABASE '{}'", export.display()))
        .unwrap();
    assert_eq!(
        restored.list_text_indexes().unwrap(),
        graph.list_text_indexes().unwrap()
    );
    assert_eq!(
        restored.fts_postings("Doc", "body", "haus").unwrap().len(),
        1
    );

    std::fs::remove_file(export.join("text_indexes.json")).unwrap();
    let legacy = Graph::open(&dir.path().join("legacy"), 1).unwrap();
    legacy
        .query(&format!("IMPORT DATABASE '{}'", export.display()))
        .unwrap();
    assert_eq!(legacy.list_text_indexes().unwrap()[0].2, Language::English);
}

use criterion::{Criterion, criterion_group, criterion_main};
use issundb_cypher::parser;

fn bench_parse_simple_match(c: &mut Criterion) {
    let query = "MATCH (n:Person) RETURN n";
    c.bench_function("parse_simple_match", |b| {
        b.iter(|| criterion::black_box(parser::parse(criterion::black_box(query)).unwrap()));
    });
}

fn bench_parse_with_where(c: &mut Criterion) {
    let query = "MATCH (n:Person)-[r:KNOWS]->(m:Person) WHERE n.age > 25 RETURN n.name, m.name";
    c.bench_function("parse_with_where", |b| {
        b.iter(|| criterion::black_box(parser::parse(criterion::black_box(query)).unwrap()));
    });
}

fn bench_parse_aggregation(c: &mut Criterion) {
    let query =
        "MATCH (n:Person)-[r:KNOWS]->(m) RETURN n.name, count(m) ORDER BY count(m) DESC LIMIT 10";
    c.bench_function("parse_aggregation", |b| {
        b.iter(|| criterion::black_box(parser::parse(criterion::black_box(query)).unwrap()));
    });
}

criterion_group!(
    benches,
    bench_parse_simple_match,
    bench_parse_with_where,
    bench_parse_aggregation,
);
criterion_main!(benches);

use std::borrow::Cow;
use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use urly::Path;
use urly::quoting::{Component, quote, requote, unquote};

fn group(
    c: &mut Criterion,
    name: &str,
    inputs: &[(&str, String)],
    op: impl Fn(&str) -> Cow<'_, str>,
) {
    let mut group = c.benchmark_group(name);
    for (name, src) in inputs {
        group.throughput(Throughput::Bytes(src.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), src, |b, src| {
            b.iter(|| black_box(op(black_box(src))));
        });
    }
    group.finish();
}

fn quoting(c: &mut Criterion) {
    let ascii = [
        ("short", "/api/v1/item".to_owned()),
        ("medium", format!("/{}", "resource".repeat(8))),
        ("long", format!("/{}", "resource".repeat(128))),
    ];
    let mut inputs = ascii.to_vec();
    inputs.extend([
        ("short_space", "/a b".to_owned()),
        ("short_unicode", "/\u{fc}".to_owned()),
        ("head_space", format!(" {}", "resource".repeat(128))),
        (
            "sparse_spaces",
            format!("/{} ", "resource".repeat(8)).repeat(16),
        ),
        ("dense_spaces", "/a b ".repeat(128)),
        ("unicode", "/\u{fc}/\u{1f642}".repeat(64)),
    ]);
    group(c, "quote_path", &inputs, |s| quote(s, Component::Path));
    group(
        c,
        "quote_query_part",
        &[
            ("short", "a+b=c&d;e".to_owned()),
            ("dense", "a+b=c&d;e".repeat(128)),
            ("sparse", format!("{} &", "resource".repeat(8)).repeat(16)),
        ],
        |s| quote(s, Component::QueryPart),
    );
    group(
        c,
        "requote_path",
        &[
            ("short", "/a%2Fb%20c".to_owned()),
            ("short_change", "/%7e".to_owned()),
            (
                "canonical",
                format!("/{}%2F%20", "resource".repeat(8)).repeat(16),
            ),
            (
                "sparse_changes",
                format!("/{}%7e", "resource".repeat(8)).repeat(16),
            ),
            ("dense_changes", "/%7e%41%2f%2b".repeat(64)),
        ],
        |s| requote(s, Component::Path),
    );

    let mut inputs = ascii.to_vec();
    inputs.extend([
        ("short_escape", "/a%20b".to_owned()),
        ("head_escape", format!("%20{}", "resource".repeat(128))),
        (
            "sparse_escapes",
            format!("/{}%20", "resource".repeat(8)).repeat(16),
        ),
        ("dense_escapes", "/%41%2F%C3%BC".repeat(64)),
        ("malformed", "/%zz/%4/100%".repeat(64)),
        ("invalid_short", "%FF".to_owned()),
        ("invalid_tail", format!("/{}%FF", "resource".repeat(128))),
        (
            "invalid_sparse",
            format!("/{}%FF", "resource".repeat(8)).repeat(16),
        ),
        ("invalid_dense", "%FF%80%C3%BC".repeat(64)),
        (
            "invalid_literal_unicode",
            "%FF\u{e9}\u{20ac}\u{1f600}%20".repeat(64),
        ),
    ]);
    group(c, "unquote_path", &inputs, |s| unquote(s, Component::Path));

    let inputs = [
        ("short", "a+b%26c".to_owned()),
        ("sparse", format!("{}+%26", "resource".repeat(8)).repeat(16)),
        ("dense", "a+b%26c%3D%2b".repeat(64)),
        ("invalid", "a+%FF%26\u{fc}%3d".repeat(64)),
    ];
    group(c, "unquote_query", &inputs, |s| {
        unquote(s, Component::Query)
    });
    group(c, "unquote_query_part", &inputs, |s| {
        unquote(s, Component::QueryPart)
    });

    let mut group = c.benchmark_group("unquote_safe_path");
    for (name, src) in [
        ("short", "/a%2Fb%25".to_owned()),
        (
            "sparse",
            format!("/{}%2f%25", "resource".repeat(8)).repeat(16),
        ),
        ("invalid", "/%FF%2f%25%C3%BC".repeat(64)),
    ] {
        let path = Path::new(&src).unwrap();
        group.throughput(Throughput::Bytes(src.len() as u64));
        group.bench_function(BenchmarkId::from_parameter(name), |b| {
            b.iter(|| black_box(black_box(path).decode_safe()));
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(500))
        .sample_size(50);
    targets = quoting
}
criterion_main!(benches);

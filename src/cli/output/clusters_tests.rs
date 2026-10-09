use super::{ClusterMember, clusters, covered_lines};
use crate::cli::{
    args::{Detail, Format},
    output::{
        cross::{CrossOutput, print_cross},
        render::Presentation,
        test_support::edge,
    },
};
use serde_json::{Value, json};

fn span(id: &str, path: &str, start: u64, end: u64) -> Value {
    json!({"id": id, "path": path, "qualifiedName": id,
        "startLine": start, "endLine": end, "endColumn": 2})
}

#[test]
fn clusters_rank_by_similarity_times_coverage_before_limiting() {
    let rows = vec![
        edge("a", "b", 0.99),
        edge("b", "c", 0.99),
        json!({"source": span("x", "x.rs", 1, 10), "matches": [
            {"function": span("y", "y.rs", 1, 10), "similarity": 0.8}]}),
        json!({"source": span("m", "m.rs", 1, 10), "matches": [
            {"function": span("n", "n.rs", 1, 10), "similarity": 0.9}]}),
    ];
    let grouped = clusters(&rows, true);
    assert_eq!(grouped[0].rank(), 18.0);
    assert_eq!(grouped[1].rank(), 16.0);
    assert_eq!(grouped[2].members.len(), 3);
    assert_eq!(grouped[2].lines, 3);
    assert_eq!(grouped[2].max, 0.99);

    let mut out = Vec::new();
    print_cross(
        &mut out,
        rows,
        CrossOutput::new(Format::Clusters, true, false, Some(1), Detail::Compact),
        &mut Presentation::empty(),
        None,
    )
    .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(output.contains("*** Cluster 1 · 2 symbols · 20 lines · similarity 0.90"));
    assert!(output.contains("m.rs:1-10:m\n"));
    assert!(!output.contains("Cluster 2"));
}

#[test]
fn equal_cluster_ranks_use_similarity_then_stable_location_order() {
    let mut rows = vec![
        json!({"source": span("y", "y.rs", 1, 2), "matches": [
            {"function": span("z", "z.rs", 1, 2), "similarity": 0.75}]}),
        json!({"source": span("h", "h.rs", 1, 2), "matches": [
            {"function": span("i", "i.rs", 1, 1), "similarity": 1.0}]}),
        json!({"source": span("a", "a.rs", 1, 2), "matches": [
            {"function": span("b", "b.rs", 1, 2), "similarity": 0.75}]}),
    ];
    for _ in 0..2 {
        let grouped = clusters(&rows, true);
        assert!(grouped.iter().all(|cluster| cluster.rank() == 3.0));
        assert_eq!(
            grouped
                .iter()
                .map(|cluster| cluster.members[0].function["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["h", "a", "y"]
        );
        rows.reverse();
    }
}

#[test]
fn coverage_merges_nested_and_overlapping_ranges_per_file_and_index() {
    let members: Vec<_> = [
        span("outer", "same.rs", 1, 10),
        span("nested", "same.rs", 3, 5),
        span("overlap", "same.rs", 8, 14),
        span("disjoint", "same.rs", 20, 22),
        span("other", "other.rs", 1, 10),
    ]
    .into_iter()
    .map(|function| ClusterMember {
        function,
        role: "index",
    })
    .collect();
    assert_eq!(covered_lines(&members), 27);

    let source = ClusterMember {
        function: span("same", "same.rs", 1, 10),
        role: "source",
    };
    let target = ClusterMember {
        function: source.function.clone(),
        role: "target",
    };
    assert_eq!(covered_lines(&[source, target]), 20);
}

#[test]
fn coverage_respects_exclusive_ends_and_tolerates_missing_or_invalid_positions() {
    let members: Vec<_> = [
        json!({"path": "exclusive.rs", "startLine": 2, "endLine": 5, "endColumn": 1}),
        json!({"path": "single.rs", "startLine": 2, "endLine": 2, "endColumn": 1}),
        json!({"path": "missing-end.rs", "startLine": 5}),
        json!({"path": "missing.rs"}),
        json!({"path": "reversed.rs", "startLine": 5, "endLine": 2}),
        json!({"path": "zero.rs", "startLine": 0, "endLine": 0}),
    ]
    .into_iter()
    .map(|function| ClusterMember {
        function,
        role: "index",
    })
    .collect();
    assert_eq!(covered_lines(&members), 8);

    let huge = ClusterMember {
        function: span("huge", "huge.rs", 1, u64::MAX),
        role: "index",
    };
    assert_eq!(covered_lines(&[huge.clone(), huge]), u64::MAX);
}

#[test]
fn clusters_are_transitive_and_limit_applies_after_components_form() {
    let rows = vec![
        edge("a", "b", 0.91),
        edge("c", "d", 0.95),
        edge("b", "c", 0.92),
        edge("x", "y", 0.99),
    ];
    let grouped = clusters(&rows, true);
    assert_eq!(grouped.len(), 2);
    assert_eq!(grouped[0].members.len(), 4);
    assert_eq!((grouped[0].min, grouped[0].max), (0.91, 0.95));
    let mut out = Vec::new();
    print_cross(
        &mut out,
        rows,
        CrossOutput::new(Format::Clusters, true, false, Some(1), Detail::Compact),
        &mut Presentation::empty(),
        None,
    )
    .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(output.contains("*** Cluster 1 · 4 symbols · 4 lines · similarity 0.91-0.95"));
    assert!(output.contains(concat!(
        "src/a.rs:1:a\n",
        "src/b.rs:1:b\n",
        "src/c.rs:1:c\n",
        "src/d.rs:1:d\n"
    )));
    assert!(!output.contains("Cluster 2"));
    assert_eq!(clusters(&[edge("a", "a", 0.9)], false)[0].members.len(), 2);
}

#[test]
fn cross_repository_clusters_do_not_merge_swapped_or_identical_node_ids() {
    let rows = vec![edge("a", "b", 0.8), edge("b", "a", 0.9)];
    let grouped = clusters(&rows, false);
    assert_eq!(grouped.len(), 2);
    assert_eq!(
        grouped[0]
            .members
            .iter()
            .map(ClusterMember::label)
            .collect::<Vec<_>>(),
        ["[source] src/b.rs:1:1 :: b", "[target] src/a.rs:1:1 :: a"]
    );
    assert_eq!(
        grouped[1]
            .members
            .iter()
            .map(ClusterMember::label)
            .collect::<Vec<_>>(),
        ["[source] src/a.rs:1:1 :: a", "[target] src/b.rs:1:1 :: b"]
    );
    assert_eq!((grouped[0].min, grouped[0].max), (0.9, 0.9));
    assert_eq!((grouped[1].min, grouped[1].max), (0.8, 0.8));
    assert_eq!(clusters(&rows, true).len(), 1);
    let mut same_id = edge("a", "a", 0.7);
    same_id["source"]["id"] = json!(0);
    same_id["matches"][0]["function"]["id"] = json!(0);
    let mut out = Vec::new();
    print_cross(
        &mut out,
        vec![same_id],
        CrossOutput::new(Format::Clusters, false, false, None, Detail::Compact),
        &mut Presentation::empty(),
        None,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        concat!(
            "*** Cluster 1 · 2 symbols · 2 lines · similarity 0.70\n",
            "src/a.rs:\n",
            "  1:a [source]\n",
            "  1:a [target]\n"
        )
    );
}

#[test]
fn clusters_distinguish_missing_id_locations_and_ignore_self_edges() {
    let a = json!({"path": "same.rs", "name": "overload", "startLine": 4, "startColumn": 1});
    let b = json!({"id": null, "path": "same.rs", "name": "overload", "startLine": 4, "startColumn": 9});
    let c = json!({"path": "same.rs", "name": "overload", "startLine": 8, "startColumn": 1});
    let rows = vec![
        json!({"source": a, "matches": [
            {"function": a, "similarity": 1.0}, {"function": b, "similarity": 0.6}]}),
        json!({"source": b, "matches": [
            {"function": a, "similarity": 0.6}, {"function": c, "similarity": 0.8, "descriptionSimilarity": 0.7}]}),
        edge("orphan", "orphan", 1.0),
    ];
    let grouped = clusters(&rows, true);
    assert_eq!(grouped.len(), 1);
    assert_eq!(
        grouped[0]
            .members
            .iter()
            .map(ClusterMember::label)
            .collect::<Vec<_>>(),
        [
            "same.rs:4:1 :: overload",
            "same.rs:4:9 :: overload",
            "same.rs:8:1 :: overload"
        ]
    );
    assert_eq!((grouped[0].min, grouped[0].max), (0.6, 0.8));
    let mut expected = Vec::new();
    print_cross(
        &mut expected,
        rows.clone(),
        CrossOutput::new(Format::Clusters, true, false, None, Detail::Expanded),
        &mut Presentation::empty(),
        None,
    )
    .unwrap();
    let output = String::from_utf8_lossy(&expected);
    assert_eq!(output.matches("  4:overload\n").count(), 2);
    assert_eq!(output.matches("same.rs:\n").count(), 1);
    assert!(output.contains("same.rs:\n  4:overload\n  4:overload\n  8:overload\n"));
    let mut reversed = rows;
    reversed.reverse();
    let mut actual = Vec::new();
    print_cross(
        &mut actual,
        reversed,
        CrossOutput::new(Format::Clusters, true, false, None, Detail::Expanded),
        &mut Presentation::empty(),
        None,
    )
    .unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn cluster_members_in_one_file_follow_numeric_source_lines() {
    let first = json!({"id": 1, "path":"same.rs", "name":"first", "startLine":2, "startColumn":1});
    let second =
        json!({"id": 2, "path":"same.rs", "name":"second", "startLine":10, "startColumn":1});
    let rows = vec![json!({"source": second, "matches": [{"function": first, "similarity":0.9}]})];
    let cluster = clusters(&rows, true).remove(0);
    assert_eq!(cluster.members[0].function["startLine"], 2);
    assert_eq!(cluster.members[1].function["startLine"], 10);
    let mut output = Vec::new();
    print_cross(
        &mut output,
        rows,
        CrossOutput::new(Format::Clusters, true, false, None, Detail::Compact),
        &mut Presentation::empty(),
        None,
    )
    .unwrap();
    let output = String::from_utf8(output).unwrap();
    assert_eq!(
        output,
        "*** Cluster 1 · 2 symbols · 2 lines · similarity 0.90\nsame.rs:\n  2:first\n  10:second\n"
    );
}

#[test]
fn cluster_locations_use_ranges_only_for_multiline_symbols() {
    let rows = vec![json!({"source": {"id": 1, "path": "src/auth/session.ts",
        "qualifiedName": "Session.validate", "startLine": 5, "endLine": 10},
        "matches": [{"function": {"id": 2, "path": "src/api/routes.ts",
            "qualifiedName": "validateSession", "startLine": 12, "endLine": 12},
            "similarity": 0.91}]})];
    for detail in [Detail::Compact, Detail::Expanded] {
        let mut out = Vec::new();
        print_cross(
            &mut out,
            rows.clone(),
            CrossOutput::new(Format::Clusters, true, false, None, detail),
            &mut Presentation::empty(),
            None,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                "*** Cluster 1 · 2 symbols · 7 lines · similarity 0.91\n",
                "src/api/routes.ts:12:validateSession\n",
                "src/auth/session.ts:5-10:Session.validate\n"
            )
        );
    }
}

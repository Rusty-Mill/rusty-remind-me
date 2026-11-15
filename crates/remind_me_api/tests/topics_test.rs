//! Coverage for `GET /api/topics`, the graph's zoom step over the topic tree.

mod common;
use common::{get, seeded_server, server};
use remind_me_core::topic::{self, Parent, SetInput, Status, TopicName};

fn make(store: &remind_me_core::db::Store<'_>, label: &str, parent: Parent) {
    topic::set(
        store,
        &SetInput {
            topic: TopicName::parse(label).unwrap(),
            parent: Some(parent),
            status: Some(Status::Active),
            evidence: Vec::new(),
            because: Some("test".into()),
            confirmed_by: "human".into(),
        },
    )
    .unwrap();
}

#[test]
fn an_empty_store_has_no_top_level_topics() {
    let (server, root) = server("topics-empty");
    let response = get(&server, "/api/topics");
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["parent"], "root");
    assert_eq!(response.json()["topics"].as_array().unwrap().len(), 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn topics_zoom_one_level_at_a_time() {
    let (server, root) = seeded_server("topics-levels", |store| {
        make(store, "Engineering", Parent::Root);
        make(
            store,
            "Systems Engineering",
            Parent::Topic(TopicName::parse("Engineering").unwrap()),
        );
    });

    let top = get(&server, "/api/topics").json();
    assert_eq!(top["topics"][0]["label"], "Engineering");
    assert_eq!(top["topics"][0]["children"], 1);

    let below = get(&server, "/api/topics?parent=Engineering").json();
    assert_eq!(below["parent"], "topic:Engineering");
    assert_eq!(below["topics"][0]["label"], "Systems Engineering");

    assert_eq!(get(&server, "/api/topics?parent=Nowhere").status, 404);
    std::fs::remove_dir_all(&root).unwrap();
}

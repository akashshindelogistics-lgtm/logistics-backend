//! Retrieval evaluation for the "ask your data" assistant: a fixture org, a
//! set of questions each mapped to the sources it must find, and recall@8
//! checked in `cargo test`. No Claude call is made, so CI catches a retrieval
//! regression. See `docs/rag-integration.md`, "Evaluation".
//!
//! Add a question here whenever retrieval misses one in real use.

use crate::logistics::ai::chunk::{self, ChunkKind};
use crate::logistics::orgs::orgs::Organization;
use crate::logistics::test_support::TestDb;
use uuid::Uuid;

const TOP_K: u32 = 8;

/// `(kind, source_id, text)`, worded like the real chunk builders in
/// `chunk.rs`.
const FIXTURE: &[(ChunkKind, &str, &str)] = &[
    (
        ChunkKind::OrgDirectory,
        "vehicle:MH12DM9387",
        "Vehicle MH12DM9387 is a truck with a capacity of 10 tonnes. Assigned driver: Ravi Kumar (active).",
    ),
    (
        ChunkKind::OrgDirectory,
        "vehicle:KA01AB1234",
        "Vehicle KA01AB1234 is a truck with a capacity of 8 tonnes. No driver currently assigned.",
    ),
    (
        ChunkKind::OrgDirectory,
        "vehicle:GJ05CD7788",
        "Vehicle GJ05CD7788 is a tempo with a capacity of 2 tonnes. Assigned driver: Meena Joshi (active).",
    ),
    (
        ChunkKind::VehicleDocument,
        "doc-ins-mh12",
        "Vehicle MH12DM9387's Insurance (document #INS-9911) expires on 2026-12-01 (status: ExpiringSoon).",
    ),
    (
        ChunkKind::VehicleDocument,
        "doc-permit-ka01",
        "Vehicle KA01AB1234's National Permit (document #NP-5521) expires on 2027-03-15 (status: Valid). Notes: covers Gujarat.",
    ),
    (
        ChunkKind::VehicleMaintenance,
        "maint-gj05",
        "Vehicle GJ05CD7788's brake pad replacement is due on 2026-11-20 (status: Scheduled).",
    ),
    (
        ChunkKind::DispatchNarrative,
        "disp-cement-pune",
        "Dispatch 1a2b3c4d to Shree Traders (4 Market Rd, Pune): Cement (200 units) via vehicle MH12DM9387. Current status: InTransit. History: Pending at 1, InTransit at 2.",
    ),
    (
        ChunkKind::DispatchNarrative,
        "disp-sand-nashik",
        "Dispatch 5e6f7a8b to Patil Builders (9 Ring Rd, Nashik): Sand (50 units) via vehicle KA01AB1234. Current status: Delivered. History: Pending at 1, Delivered at 3. Delivered to Suresh Patil.",
    ),
    (
        ChunkKind::DispatchNarrative,
        "disp-tiles-surat",
        "Dispatch 9c0d1e2f to Gupta Tiles (2 Mill Rd, Surat): Tiles (80 units) via vehicle GJ05CD7788. Current status: Returned. History: Pending at 1, Returned at 4.",
    ),
    (
        ChunkKind::Notification,
        "notif-late",
        "Notification to admin via Email (Sent): Dispatch 1a2b3c4d is running late",
    ),
    (
        ChunkKind::Notification,
        "notif-dispatched",
        "Notification to customer via Sms (Sent): Hi Priya Sharma, your order (ref 5e6f7a8b) has been dispatched.",
    ),
    (
        ChunkKind::StockSnapshot,
        "stock-cement-pune",
        "Godown 'Pune Depot' (12 Mill Rd, Pune) holds 40 units of Cement (category: Building) \u{2014} below its reorder threshold.",
    ),
    (
        ChunkKind::StockSnapshot,
        "stock-pallets-chennai",
        "Godown 'Chennai Central' (12 Anna Salai, Chennai) holds 900 units of Pallets (category: General).",
    ),
    (
        ChunkKind::OrgDirectory,
        "customer:shree",
        "Customer 'Shree Traders' is located at 4 Market Rd, Pune. Contact: 9822000011, shree@example.com.",
    ),
    (
        ChunkKind::OrgDirectory,
        "customer:patil",
        "Customer 'Patil Builders' is located at 9 Ring Rd, Nashik.",
    ),
    (
        ChunkKind::OrgDirectory,
        "godown:pune",
        "Godown 'Pune Depot' is located at 12 Mill Rd, Pune. Maximum capacity: 5000.",
    ),
];

/// A question and the sources that must be in the top 8 for it.
const QUESTIONS: &[(&str, &[&str])] = &[
    // The measured misses from the plan.
    (
        "MH12DM9387",
        &["vehicle:MH12DM9387", "doc-ins-mh12", "disp-cement-pune"],
    ),
    (
        "9387",
        &["vehicle:MH12DM9387", "doc-ins-mh12", "disp-cement-pune"],
    ),
    (
        "MH 12 DM 9387",
        &["vehicle:MH12DM9387", "doc-ins-mh12", "disp-cement-pune"],
    ),
    ("mh-12-dm-9387", &["vehicle:MH12DM9387"]),
    ("DM9387", &["vehicle:MH12DM9387"]),
    ("cement", &["disp-cement-pune", "stock-cement-pune"]),
    ("cements", &["disp-cement-pune", "stock-cement-pune"]),
    ("delayed", &["notif-late"]),
    ("late", &["notif-late"]),
    ("which dispatches are running late", &["notif-late"]),
    // Ordinary questions that already worked and must keep working.
    ("insurance expiring", &["doc-ins-mh12"]),
    ("tiles returned from Surat", &["disp-tiles-surat"]),
    ("Who is the driver of 7788", &["vehicle:GJ05CD7788"]),
    ("lorry with no driver", &["vehicle:KA01AB1234"]),
    ("Which permit covers Gujarat?", &["doc-permit-ka01"]),
    ("what is low on stock", &["stock-cement-pune"]),
    (
        "inventory of pallets in Chennai",
        &["stock-pallets-chennai"],
    ),
    ("Shree Traders phone", &["customer:shree"]),
    ("who received the sand delivery", &["disp-sand-nashik"]),
    ("brakes due for 7788", &["maint-gj05"]),
];

fn seed(org: Uuid) {
    for (kind, source_id, text) in FIXTURE {
        chunk::upsert(org, *kind, source_id, (*text).to_string()).expect("seed chunk");
    }
}

/// Source ids in the top 8 for `question`.
fn top_sources(org: Uuid, question: &str) -> Vec<String> {
    chunk::search_by_org(org, question, TOP_K)
        .expect("search")
        .into_iter()
        .map(|c| c.source_id)
        .collect()
}

#[test]
fn every_question_finds_all_of_its_expected_sources_in_the_top_8() {
    let _db = TestDb::create();
    let org = Organization::create_organization("Eval Org", "1 Eval Rd").unwrap();
    seed(org.id);

    let (mut found, mut expected_total) = (0usize, 0usize);
    let mut misses = Vec::new();
    for (question, expected) in QUESTIONS {
        let got = top_sources(org.id, question);
        for want in *expected {
            expected_total += 1;
            if got.iter().any(|g| g == want) {
                found += 1;
            } else {
                misses.push(format!("{question:?} missed {want} (got {got:?})"));
            }
        }
    }
    let recall = found as f64 / expected_total as f64;
    println!("recall@{TOP_K} = {recall:.3} ({found}/{expected_total})");
    assert!(
        misses.is_empty(),
        "recall@{TOP_K} {recall:.3}; misses:\n{}",
        misses.join("\n")
    );
}

#[test]
fn another_orgs_chunks_never_appear() {
    let _db = TestDb::create();
    let org = Organization::create_organization("Eval Org", "1 Eval Rd").unwrap();
    let other = Organization::create_organization("Other Org", "2 Eval Rd").unwrap();
    seed(org.id);
    chunk::upsert(
        other.id,
        ChunkKind::OrgDirectory,
        "vehicle:MH12DM9387",
        "Vehicle MH12DM9387 is a truck with cement for Rival Traders.".to_string(),
    )
    .unwrap();
    for q in ["9387", "MH12DM9387", "cements", "Rival Traders"] {
        let got = chunk::search_by_org(org.id, q, TOP_K).unwrap();
        assert!(
            got.iter()
                .all(|c| c.org_id == org.id && !c.text.contains("Rival")),
            "{q}: {got:?}"
        );
    }
}

// NOTE: This file is auto-generated by Diesel.
// Run `diesel migration run` to update (in src/fog/sql_recovery_db)
#![allow(unused_imports)]

table! {
    use diesel::sql_types::*;
    use crate::sql_types::*;

    ingest_invocations (id) {
        id -> Int8,
        ingress_public_key -> Bytea,
        egress_public_key -> Bytea,
        last_active_at -> Timestamp,
        start_block -> Int8,
        decommissioned -> Bool,
        rng_version -> Int4,
    }
}

table! {
    use diesel::sql_types::*;
    use crate::sql_types::*;

    ingested_blocks (id) {
        id -> Int8,
        ingest_invocation_id -> Int8,
        ingress_public_key -> Bytea,
        block_number -> Int8,
        cumulative_txo_count -> Int8,
        block_signature_timestamp -> Int8,
        proto_ingested_block_data -> Bytea,
    }
}

table! {
    use diesel::sql_types::*;
    use crate::sql_types::*;

    ingress_keys (ingress_public_key) {
        ingress_public_key -> Bytea,
        start_block -> Int8,
        pubkey_expiry -> Int8,
        retired -> Bool,
    }
}

table! {
    use diesel::sql_types::*;
    use crate::sql_types::*;

    reports (id) {
        id -> Int8,
        ingress_public_key -> Bytea,
        ingest_invocation_id -> Nullable<Int8>,
        fog_report_id -> Varchar,
        report -> Bytea,
        pubkey_expiry -> Int8,
    }
}

table! {
    use diesel::sql_types::*;
    use crate::sql_types::*;

    user_events (id) {
        id -> Int8,
        event_type -> User_event_type,
        new_ingest_invocation_id -> Nullable<Int8>,
        decommission_ingest_invocation_id -> Nullable<Int8>,
        missing_blocks_start -> Nullable<Int8>,
        missing_blocks_end -> Nullable<Int8>,
    }
}

joinable!(ingested_blocks -> ingest_invocations (ingest_invocation_id));
joinable!(reports -> ingest_invocations (ingest_invocation_id));
joinable!(reports -> ingress_keys (ingress_public_key));

allow_tables_to_appear_in_same_query!(
    ingest_invocations,
    ingested_blocks,
    ingress_keys,
    reports,
    user_events,
);

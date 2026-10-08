// Copyright (c) Microsoft Corporation. All rights reserved.

use super::catalog::{text, unsigned};
use super::storage::{Directory, allocated_bytes};
use super::{CatalogCursor, Error, Id, Measurement, Namespace, ObjectRecord, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(super) struct Counters(BTreeMap<String, u64>);

impl Counters {
    pub(super) fn get(&self, key: &str) -> Result<u64> {
        self.0
            .get(key)
            .copied()
            .ok_or_else(|| Error::corrupt(format!("missing storage counter: {key}")))
    }
}

pub(super) fn counters(connection: &Connection) -> Result<Counters> {
    let mut statement =
        connection.prepare("SELECT name,value FROM namespace_totals ORDER BY name LIMIT 64")?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, unsigned(row, 1)?)))?;
    let counters = Counters(rows.collect::<std::result::Result<_, _>>()?);
    let required = [
        "unconsumed_staging",
        "uncertain_staging",
        "object_logical",
        "object_allocated",
        "object_unavailable",
        "object_protected",
        "object_pending",
        "generations_published",
        "generations_retired",
        "checkpoints",
        "preparations",
        "quarantined",
        "control_logical",
        "control_allocated",
        "control_unavailable",
        "compatibility_logical",
        "compatibility_allocated",
        "compatibility_unavailable",
        "compatibility_generations",
        "reservations",
        "reserved_staging",
        "reserved_private",
        "reserved_slots",
    ];
    if counters.0.len() != required.len() {
        return Err(Error::corrupt("storage counter schema differs"));
    }
    for key in required {
        counters.get(key)?;
    }
    Ok(counters)
}

fn counter_triggers(
    connection: &Connection,
    table: &str,
    condition: &str,
    metrics: &[(&str, &str)],
    before_new: &str,
    after_old: &str,
) -> Result<()> {
    for (name, _) in metrics {
        connection.execute(
            "INSERT OR IGNORE INTO namespace_totals VALUES(?1,0)",
            [name],
        )?;
    }
    for event in ["INSERT", "UPDATE", "DELETE"] {
        let value = |expression: &str, row: &str| {
            format!(
                "CASE WHEN {} THEN ({}) ELSE 0 END",
                condition.replace("{r}", row),
                expression.replace("{r}", row)
            )
        };
        let mut body = String::new();
        for (name, expression) in metrics {
            let old = if event == "INSERT" {
                "0".into()
            } else {
                value(expression, "OLD")
            };
            let new = if event == "DELETE" {
                "0".into()
            } else {
                value(expression, "NEW")
            };
            body.push_str(&format!(
                "UPDATE namespace_totals SET value=value-({old})+({new}) WHERE name='{name}';"
            ));
        }
        if event != "INSERT" {
            body.push_str(after_old);
        }
        if event != "DELETE" {
            body.push_str(before_new);
        }
        let when = match event {
            "INSERT" => condition.replace("{r}", "NEW"),
            "DELETE" => condition.replace("{r}", "OLD"),
            _ => format!(
                "({}) OR ({})",
                condition.replace("{r}", "OLD"),
                condition.replace("{r}", "NEW")
            ),
        };
        connection.execute_batch(&format!("CREATE TRIGGER tally_{table}_{event} AFTER {event} ON {table} WHEN {when} BEGIN {body} END;"))?;
    }
    Ok(())
}

/// Transactional, constant-size summaries avoid catalog-wide scans on query
/// status, admission and scheduler ticks. The source rows remain authoritative.
pub(super) fn install_counters(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE namespace_totals (
           name TEXT PRIMARY KEY,
           value INTEGER NOT NULL CHECK(typeof(value)='integer' AND value>=0)
         );
         CREATE TABLE operation_storage (
           id TEXT PRIMARY KEY, logical INTEGER NOT NULL CHECK(logical>=0),
           objects INTEGER NOT NULL CHECK(objects>=0),
           unsealed INTEGER NOT NULL CHECK(unsealed>=0),
           floor INTEGER NOT NULL CHECK(floor>=0)
         );
         CREATE UNIQUE INDEX reservations_operation ON reservations(operation_id);
         CREATE INDEX objects_operation ON objects(json_extract(record,'$.operation'),id) WHERE state!='removed';",
    )?;
    counter_triggers(
        connection,
        "operation_storage",
        "1",
        &[
            (
                "unconsumed_staging",
                "-min({r}.logical,coalesce((SELECT bytes FROM reservations WHERE operation_id={r}.id),0))",
            ),
            (
                "uncertain_staging",
                "CASE WHEN NOT EXISTS(SELECT 1 FROM reservations WHERE operation_id={r}.id)
          AND {r}.unsealed>0 THEN max(0,{r}.floor-{r}.logical) ELSE 0 END",
            ),
        ],
        "",
        "",
    )?;
    let add_operation = "
        INSERT INTO operation_storage(id,logical,objects,unsealed,floor)
        SELECT json_extract(NEW.record,'$.operation'),json_extract(NEW.record,'$.logical_bytes'),1,
          json_extract(NEW.record,'$.accounting_floor')>0,json_extract(NEW.record,'$.accounting_floor')
        WHERE NEW.state!='removed' AND json_type(NEW.record,'$.operation')='text'
        ON CONFLICT(id) DO UPDATE SET logical=logical+excluded.logical,objects=objects+1,
          unsealed=unsealed+excluded.unsealed,floor=max(floor,excluded.floor);";
    let remove_operation = "
        UPDATE operation_storage SET logical=logical-json_extract(OLD.record,'$.logical_bytes'),
          objects=objects-1,
          floor=CASE WHEN unsealed-(json_extract(OLD.record,'$.accounting_floor')>0)=0 THEN 0 ELSE floor END,
          unsealed=unsealed-(json_extract(OLD.record,'$.accounting_floor')>0)
        WHERE OLD.state!='removed' AND id=json_extract(OLD.record,'$.operation');
        DELETE FROM operation_storage WHERE id=json_extract(OLD.record,'$.operation') AND objects=0;";
    counter_triggers(connection, "objects", "{r}.state!='removed'", &[
        ("object_logical", "json_extract({r}.record,'$.logical_bytes')"),
        ("object_allocated", "coalesce(json_extract({r}.record,'$.allocated_bytes.value'),0)"),
        ("object_unavailable", "json_extract({r}.record,'$.allocated_bytes.status')!='observed'"),
        ("object_protected", "CASE WHEN EXISTS(SELECT 1 FROM refs WHERE target={r}.id) THEN json_extract({r}.record,'$.logical_bytes') ELSE 0 END"),
        ("object_pending", "CASE WHEN {r}.state='pending-deletion' THEN json_extract({r}.record,'$.logical_bytes') ELSE 0 END"),
        ("generations_published", "{r}.kind='generation' AND {r}.state='published'"),
        ("generations_retired", "{r}.kind='generation' AND {r}.state IN ('retired','pending-deletion')"),
        ("checkpoints", "{r}.kind='checkpoint'"),
        ("preparations", "{r}.state='preparing'"),
        ("quarantined", "{r}.state='quarantined'"),
        ("uncertain_staging", "CASE WHEN json_type({r}.record,'$.operation')='null'
          THEN max(0,json_extract({r}.record,'$.accounting_floor')-json_extract({r}.record,'$.logical_bytes')) ELSE 0 END"),
    ], add_operation, remove_operation)?;
    counter_triggers(
        connection,
        "control_files",
        "json_extract({r}.record,'$.file.removed')=0",
        &[
            (
                "control_logical",
                "json_extract({r}.record,'$.file.logical_bytes')",
            ),
            (
                "control_allocated",
                "coalesce(json_extract({r}.record,'$.file.allocated_bytes.value'),0)",
            ),
            (
                "control_unavailable",
                "json_extract({r}.record,'$.file.allocated_bytes.status')!='observed'",
            ),
        ],
        "",
        "",
    )?;
    counter_triggers(
        connection,
        "records",
        "{r}.kind='compatibility-generation'",
        &[
            (
                "compatibility_logical",
                "json_extract({r}.record,'$.logical_bytes')",
            ),
            (
                "compatibility_allocated",
                "coalesce(json_extract({r}.record,'$.allocated_bytes.value'),0)",
            ),
            (
                "compatibility_unavailable",
                "json_extract({r}.record,'$.allocated_bytes.status')!='observed'",
            ),
            ("compatibility_generations", "1"),
        ],
        "",
        "",
    )?;
    counter_triggers(
        connection,
        "reservations",
        "1",
        &[
            ("reservations", "1"),
            ("reserved_staging", "{r}.bytes"),
            (
                "reserved_private",
                "json_extract({r}.record,'$.request.private_bytes')",
            ),
            ("reserved_slots", "{r}.slots"),
            (
                "unconsumed_staging",
                "max(0,{r}.bytes-coalesce((SELECT logical FROM operation_storage WHERE id={r}.operation_id),0))",
            ),
            (
                "uncertain_staging",
                "-coalesce((SELECT CASE WHEN unsealed>0 THEN max(0,floor-logical) ELSE 0 END
          FROM operation_storage WHERE id={r}.operation_id),0)",
            ),
        ],
        "",
        "",
    )?;
    connection.execute_batch(
        "CREATE TRIGGER tally_first_reference AFTER INSERT ON refs
         WHEN NOT EXISTS(SELECT 1 FROM refs WHERE target=NEW.target AND id!=NEW.id)
         BEGIN UPDATE namespace_totals SET value=value+coalesce((SELECT json_extract(record,'$.logical_bytes')
           FROM objects WHERE id=NEW.target AND state!='removed'),0) WHERE name='object_protected'; END;
         CREATE TRIGGER tally_last_reference AFTER DELETE ON refs
         WHEN NOT EXISTS(SELECT 1 FROM refs WHERE target=OLD.target)
         BEGIN UPDATE namespace_totals SET value=value-coalesce((SELECT json_extract(record,'$.logical_bytes')
           FROM objects WHERE id=OLD.target AND state!='removed'),0) WHERE name='object_protected'; END;
         CREATE TRIGGER tally_changed_reference AFTER UPDATE OF target ON refs WHEN NEW.target!=OLD.target
         BEGIN
           UPDATE namespace_totals SET value=value-coalesce((SELECT json_extract(record,'$.logical_bytes') FROM objects
             WHERE id=OLD.target AND state!='removed' AND NOT EXISTS(SELECT 1 FROM refs WHERE target=OLD.target)),0)
             +coalesce((SELECT json_extract(record,'$.logical_bytes') FROM objects WHERE id=NEW.target AND state!='removed'
             AND NOT EXISTS(SELECT 1 FROM refs WHERE target=NEW.target AND id!=NEW.id)),0)
           WHERE name='object_protected';
         END;",
    )?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageUsage {
    pub accounting_schema: u32,
    pub managed_logical_bytes: u64,
    pub control_logical_bytes: u64,
    pub catalog_logical_bytes: u64,
    pub compatibility_observed_logical_bytes: u64,
    pub known_logical_bytes: u64,
    pub known_allocated_bytes: Measurement<u64>,
    pub uncertain_staging_charge_bytes: u64,
    pub catalog_protected_logical_bytes: u64,
    pub pending_deletion_logical_bytes: u64,
    pub published_generations: u64,
    pub retired_generations: u64,
    pub checkpoints: u64,
    pub preparations: u64,
    pub quarantined_objects: u64,
    pub compatibility_generations: u64,
    pub effective_target_bytes: Option<u64>,
    pub known_budget_shortfall_bytes: Option<u64>,
    pub charged_overlap_logical_bytes: u64,
    pub charged_budget_shortfall_bytes: Option<u64>,
    pub untracked_storage_bytes: Measurement<u64>,
    pub eligible_logical_bytes: Measurement<u64>,
}

#[derive(Serialize)]
pub struct InspectedObject {
    pub catalog: ObjectRecord,
    pub current_eligibility: Option<super::Eligibility>,
    pub error: Option<serde_json::Value>,
}

#[derive(Default, Serialize)]
pub struct InspectionTotals {
    pub examined: u32,
    pub eligible_logical_bytes: u64,
    pub protected_logical_bytes: u64,
    pub uncertain_logical_bytes: u64,
    pub pending_deletion_logical_bytes: u64,
}

#[derive(Serialize)]
pub struct StorageInspection {
    pub accounting_schema: u32,
    pub catalog_revision: u64,
    pub usage: super::WorkUsage,
    pub page_totals: InspectionTotals,
    pub objects: Vec<InspectedObject>,
    pub next: Option<CatalogCursor>,
}

fn add(total: u64, value: u64) -> Result<u64> {
    total
        .checked_add(value)
        .ok_or_else(|| Error::corrupt("storage accounting overflow"))
}

pub(super) fn ledger(
    connection: &Connection,
    counters: &Counters,
    catalog_logical_bytes: u64,
    catalog_allocated_bytes: Measurement<u64>,
) -> Result<StorageUsage> {
    let logical = counters.get("object_logical")?;
    let allocated = counters.get("object_allocated")?;
    let unavailable = counters.get("object_unavailable")?;
    let uncertain = counters.get("uncertain_staging")?;
    let control_logical = counters.get("control_logical")?;
    let compatibility = counters.get("compatibility_logical")?;
    let policy: String =
        connection.query_row("SELECT policy FROM state WHERE singleton=1", [], |row| {
            row.get(0)
        })?;
    let policy: super::Policy = serde_json::from_str(&policy)?;
    let allocation = super::work::allocation_row(connection)?;
    let target = policy
        .storage_target()
        .into_iter()
        .chain(allocation.storage_bytes)
        .min();
    let known = add(
        add(add(logical, control_logical)?, catalog_logical_bytes)?,
        compatibility,
    )?;
    let known_allocated_bytes = match catalog_allocated_bytes {
        Measurement::Observed { value }
            if unavailable == 0
                && counters.get("control_unavailable")? == 0
                && counters.get("compatibility_unavailable")? == 0 =>
        {
            Measurement::Observed {
                value: add(
                    add(add(allocated, counters.get("control_allocated")?)?, value)?,
                    counters.get("compatibility_allocated")?,
                )?,
            }
        }
        _ => Measurement::Unavailable {
            reason: "one-or-more-file-allocation-measurements-unavailable".into(),
        },
    };
    let charged_overlap_logical_bytes =
        add(add(known, uncertain)?, counters.get("unconsumed_staging")?)?;
    Ok(StorageUsage {
        accounting_schema: 1,
        managed_logical_bytes: logical,
        control_logical_bytes: control_logical,
        catalog_logical_bytes,
        compatibility_observed_logical_bytes: compatibility,
        known_logical_bytes: known,
        known_allocated_bytes,
        uncertain_staging_charge_bytes: uncertain,
        catalog_protected_logical_bytes: counters.get("object_protected")?,
        pending_deletion_logical_bytes: counters.get("object_pending")?,
        published_generations: counters.get("generations_published")?,
        retired_generations: counters.get("generations_retired")?,
        checkpoints: counters.get("checkpoints")?,
        preparations: counters.get("preparations")?,
        quarantined_objects: counters.get("quarantined")?,
        compatibility_generations: counters.get("compatibility_generations")?,
        effective_target_bytes: target,
        known_budget_shortfall_bytes: target.map(|target| known.saturating_sub(target)),
        charged_overlap_logical_bytes,
        charged_budget_shortfall_bytes: target
            .map(|target| charged_overlap_logical_bytes.saturating_sub(target)),
        untracked_storage_bytes: Measurement::Unavailable {
            reason: "requires-bounded-filesystem-inventory".into(),
        },
        eligible_logical_bytes: Measurement::Unavailable {
            reason: "requires-paginated-live-protection-inspection".into(),
        },
    })
}

impl Namespace {
    pub fn inspect_storage(&self, cursor: Option<CatalogCursor>) -> Result<StorageInspection> {
        let page = self.page(cursor)?;
        let mut totals = InspectionTotals::default();
        let mut objects = Vec::with_capacity(page.objects.len());
        for object in page.objects {
            totals.examined += 1;
            let (eligibility, error) = match self.eligibility(&object.id) {
                Ok(eligibility) => {
                    if eligibility.state == super::ObjectState::PendingDeletion {
                        totals.pending_deletion_logical_bytes = add(
                            totals.pending_deletion_logical_bytes,
                            eligibility.logical_bytes,
                        )?;
                    }

                    if eligibility.eligible {
                        totals.eligible_logical_bytes =
                            add(totals.eligible_logical_bytes, eligibility.logical_bytes)?;
                    } else {
                        totals.protected_logical_bytes =
                            add(totals.protected_logical_bytes, eligibility.logical_bytes)?;
                    }
                    (Some(eligibility), None)
                }
                Err(error) => {
                    if error.category != super::ErrorCategory::CacheEvicted {
                        totals.uncertain_logical_bytes =
                            add(totals.uncertain_logical_bytes, object.logical_bytes)?;
                    }
                    (None, Some(serde_json::to_value(error)?))
                }
            };
            objects.push(InspectedObject {
                catalog: object,
                current_eligibility: eligibility,
                error,
            });
        }
        Ok(StorageInspection {
            accounting_schema: 1,
            catalog_revision: page.revision,
            usage: self.work_usage()?,
            page_totals: totals,
            objects,
            next: page.next,
        })
    }

    pub(super) fn record_compatibility_generation(
        &self,
        generation: &crate::generations::Generation,
    ) -> Result<()> {
        if self.header().storage != super::policy::StorageMode::CompatibilityRetainAll
            || generation.key().repository_identity() != self.header().repository
        {
            return Err(Error::incompatible(
                "legacy accounting requires this repository's compatibility namespace",
            ));
        }
        let directory = Directory::open(generation.directory())?;
        let mut logical = 0;
        let mut allocated = 0;
        let mut unavailable = None;
        for name in [
            "files.bin",
            "lookup.bin",
            "index.bin",
            "meta.json",
            "generation.json",
        ] {
            let file = directory.open_file(name, false)?;
            logical = add(logical, file.metadata()?.len())?;
            match allocated_bytes(&file)? {
                Measurement::Observed { value } => allocated = add(allocated, value)?,
                Measurement::Unavailable { reason } => unavailable = Some(reason),
            }
        }
        let allocated = match unavailable {
            Some(reason) => Measurement::Unavailable { reason },
            None => Measurement::Observed { value: allocated },
        };
        let record = serde_json::json!({
            "directory":super::NativePath::from_path(directory.path())?,"identity":directory.identity()?,
            "generation":generation.key(),"logical_bytes":logical,"allocated_bytes":allocated,
        });
        let key = blake3::hash(text(generation.key())?.as_bytes())
            .to_hex()
            .to_string();
        self.transaction(|transaction| {
            transaction.execute(
                "INSERT INTO records VALUES('compatibility-generation',?1,1,?2)
                 ON CONFLICT(kind,id) DO UPDATE SET version=version+1,record=excluded.record",
                params![key, text(&record)?],
            )?;
            Ok(())
        })
    }

    pub(super) fn quarantine_unsealed(&self, id: &Id, floor: u64) -> Result<()> {
        self.quarantine_object(id, floor, &Error::new(super::ErrorCategory::RecoveryRequired,
            "unsealed-storage-identity", "the interrupted object has no complete ownership seal; preserve its entries and inspect storage"))
    }

    pub(super) fn quarantine_object(&self, id: &Id, floor: u64, error: &Error) -> Result<()> {
        self.transaction(|transaction| {
            let mut object = super::catalog::object_row(transaction, id)?;
            object.state = super::ObjectState::Quarantined;
            object.accounting_floor = object.accounting_floor.max(floor);
            object.error = Some(serde_json::to_value(error)?);
            super::catalog::save_object(transaction, &mut object)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ObjectKind, ReferenceKind, Token, WorkRequest};
    use super::*;

    fn check_totals(namespace: &Namespace) {
        namespace.read(|connection| {
            let totals = counters(connection)?;
            for (name, query) in [
                ("object_logical", "SELECT coalesce(sum(json_extract(record,'$.logical_bytes')),0) FROM objects WHERE state!='removed'"),
                ("object_protected", "SELECT coalesce(sum(json_extract(record,'$.logical_bytes')),0) FROM objects WHERE state!='removed' AND EXISTS(SELECT 1 FROM refs WHERE target=objects.id)"),
                ("object_pending", "SELECT coalesce(sum(json_extract(record,'$.logical_bytes')),0) FROM objects WHERE state='pending-deletion'"),
                ("control_logical", "SELECT coalesce(sum(json_extract(record,'$.file.logical_bytes')),0) FROM control_files WHERE json_extract(record,'$.file.removed')=0"),
                ("reserved_staging", "SELECT coalesce(sum(bytes),0) FROM reservations"),
                ("unconsumed_staging", "SELECT coalesce(sum(max(0,bytes-coalesce((SELECT sum(json_extract(o.record,'$.logical_bytes')) FROM objects o WHERE o.state!='removed' AND json_extract(o.record,'$.operation')=reservations.operation_id),0))),0) FROM reservations"),
                ("reservations", "SELECT count(*) FROM reservations"),
                ("preparations", "SELECT count(*) FROM objects WHERE state='preparing'"),
                ("quarantined", "SELECT count(*) FROM objects WHERE state='quarantined'"),
            ] {
                let expected = connection.query_row(query, [], |row| unsigned(row, 0))?;
                assert_eq!(totals.get(name)?, expected, "{name}");
            }
            Ok(())
        }).unwrap();
    }

    #[test]
    fn transactional_totals_count_shared_references_and_reservations_once() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = Namespace::initialize_identity(
            &"a".repeat(64),
            temp.path(),
            super::super::policy::fixture_policy(),
        )
        .unwrap();
        namespace.activate().unwrap();
        let operation = namespace
            .accept_system_operation(
                Token::parse("accounting").unwrap(),
                "build",
                serde_json::json!({}),
            )
            .unwrap();
        let permit = namespace
            .reserve(
                &operation.id,
                None,
                WorkRequest {
                    allocation_version: 1,
                    staging_bytes: 1024 * 1024,
                    private_bytes: 1024 * 1024,
                    slots: 1,
                },
            )
            .unwrap();
        check_totals(&namespace);
        let (first, first_pin) = namespace
            .create_object(ObjectKind::CheckpointStage, None, &permit)
            .unwrap();
        let (second, second_pin) = namespace
            .create_object(ObjectKind::CheckpointStage, None, &permit)
            .unwrap();
        check_totals(&namespace);
        namespace.publish_object(&first.id, None, None).unwrap();
        let a = Id::new().unwrap();
        let b = Id::new().unwrap();
        namespace
            .transaction(|transaction| {
                Namespace::add_reference(
                    transaction,
                    &a,
                    ReferenceKind::Persistent,
                    a.as_str(),
                    &first.id,
                    None,
                )?;
                Namespace::add_reference(
                    transaction,
                    &b,
                    ReferenceKind::Persistent,
                    b.as_str(),
                    &first.id,
                    None,
                )
            })
            .unwrap();
        check_totals(&namespace);
        assert_eq!(
            namespace
                .work_usage()
                .unwrap()
                .storage
                .catalog_protected_logical_bytes,
            namespace.object(&first.id).unwrap().logical_bytes
        );
        namespace
            .transaction(|transaction| {
                transaction.execute("DELETE FROM refs WHERE id=?1", [a.as_str()])?;
                Ok(())
            })
            .unwrap();
        check_totals(&namespace);
        namespace
            .quarantine_unsealed(&first.id, permit.staging_limit())
            .unwrap();
        namespace
            .quarantine_unsealed(&second.id, permit.staging_limit())
            .unwrap();
        assert_eq!(
            namespace
                .work_usage()
                .unwrap()
                .storage
                .uncertain_staging_charge_bytes,
            0,
            "a live reservation already covers its unsealed objects"
        );
        drop(permit);
        check_totals(&namespace);
        let usage = namespace.work_usage().unwrap();
        assert_eq!(
            usage.storage.uncertain_staging_charge_bytes + usage.object_logical_bytes,
            1024 * 1024,
            "multiple uncertain objects share one operation's conservative charge"
        );
        namespace
            .transaction(|transaction| {
                transaction.execute("DELETE FROM refs WHERE id=?1", [b.as_str()])?;
                Ok(())
            })
            .unwrap();
        check_totals(&namespace);
        assert_eq!(
            namespace
                .work_usage()
                .unwrap()
                .storage
                .catalog_protected_logical_bytes,
            0
        );
        drop((first_pin, second_pin));
        let path = namespace.path().to_path_buf();
        drop(namespace);
        let namespace = Namespace::open(&path).unwrap();
        check_totals(&namespace);
        assert_eq!(
            namespace
                .work_usage()
                .unwrap()
                .storage
                .uncertain_staging_charge_bytes,
            usage.storage.uncertain_staging_charge_bytes
        );
    }

    #[test]
    fn accounting_updates_rollback_with_their_source_rows() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = Namespace::initialize_identity(
            &"a".repeat(64),
            temp.path(),
            super::super::policy::fixture_policy(),
        )
        .unwrap();
        namespace.activate().unwrap();
        let operation = namespace
            .accept_system_operation(
                Token::parse("rollback").unwrap(),
                "build",
                serde_json::json!({}),
            )
            .unwrap();
        let permit = namespace
            .reserve(
                &operation.id,
                None,
                WorkRequest {
                    allocation_version: 1,
                    staging_bytes: 1024 * 1024,
                    private_bytes: 1024 * 1024,
                    slots: 1,
                },
            )
            .unwrap();
        let (object, _pin) = namespace
            .create_object(ObjectKind::BuildStage, None, &permit)
            .unwrap();
        let before = namespace.work_usage().unwrap().object_logical_bytes;
        let error = namespace
            .transaction(|transaction| {
                let mut changed = super::super::catalog::object_row(transaction, &object.id)?;
                changed.logical_bytes += 99;
                super::super::catalog::save_object(transaction, &mut changed)?;
                Err::<(), _>(Error::invalid("injected transaction rollback"))
            })
            .unwrap_err();
        assert_eq!(error.category, super::super::ErrorCategory::InvalidInput);
        assert_eq!(namespace.work_usage().unwrap().object_logical_bytes, before);
        check_totals(&namespace);
    }
}

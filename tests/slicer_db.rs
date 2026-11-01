//! `db::slicer`: profiles, per-printer settings and the job state machine (migration 0007).

mod common;

use printpit::auth::Role;
use printpit::db::slicer::{
    self, DeleteProfile, JobStats, JobStatus, NewJob, NewProfile, PrinterSlicer, Source,
};
use printpit::db::{Db, printers, users};
use printpit::slicer::{Kind, Profile};
use serde_json::json;

fn settings(v: serde_json::Value) -> Profile {
    v.as_object().unwrap().clone()
}

async fn db() -> (common::TestApp, Db) {
    let app = common::TestApp::new().await;
    let db = app.state.db.clone();
    (app, db)
}

async fn user(db: &Db, name: &str) -> i64 {
    users::create(db, name, "x", Role::Operator).await.unwrap()
}

async fn printer(db: &Db, name: &str) -> i64 {
    printers::create(
        db,
        printers::NewPrinter {
            name,
            base_url: "http://10.0.0.5:7125",
            api_key_enc: None,
            webcam_override: None,
            webcam_transform: None,
            enabled: true,
            notify: false,
        },
    )
    .await
    .unwrap()
}

async fn profile(db: &Db, kind: Kind, name: &str) -> i64 {
    slicer::create_profile(
        db,
        &NewProfile {
            kind,
            name,
            source: Source::System,
            vendor: Some("Acme"),
            settings: &settings(json!({"name": name, "layer_height": "0.2"})),
        },
    )
    .await
    .unwrap()
}

fn job<'a>(user_id: i64, printer_id: i64, machine: i64, process: i64, filament: i64) -> NewJob<'a> {
    NewJob {
        user_id,
        printer_id,
        model_name: "benchy.stl",
        model_size: 1234,
        machine_profile_id: Some(machine),
        process_id: Some(process),
        filament_id: Some(filament),
        options_json: r#"{"infill":20}"#,
        start_print: false,
    }
}

#[tokio::test]
async fn profiles_round_trip() {
    let (_app, db) = db().await;
    let m = profile(&db, Kind::Machine, "Acme One").await;
    let p = profile(&db, Kind::Process, "0.20mm Standard").await;
    let f = profile(&db, Kind::Filament, "Generic PLA").await;
    // Same name, different kind: fine. Same kind and name (any case): not.
    profile(&db, Kind::Process, "Acme One").await;
    let dup = slicer::create_profile(
        &db,
        &NewProfile {
            kind: Kind::Machine,
            name: "acme one",
            source: Source::User,
            vendor: None,
            settings: &Profile::new(),
        },
    )
    .await;
    assert!(dup.is_err());

    let all = slicer::list_profiles(&db, None).await.unwrap();
    let order: Vec<_> = all.iter().map(|p| (p.kind.as_str(), p.name.as_str())).collect();
    assert_eq!(
        order,
        [
            ("machine", "Acme One"),
            ("process", "0.20mm Standard"),
            ("process", "Acme One"),
            ("filament", "Generic PLA")
        ]
    );
    let machines = slicer::list_profiles(&db, Some(Kind::Machine)).await.unwrap();
    assert_eq!(machines.len(), 1);
    assert_eq!(machines[0].kind(), Some(Kind::Machine));

    let got = slicer::get_profile(&db, p).await.unwrap().unwrap();
    assert_eq!(got.settings["layer_height"], "0.2");
    assert_eq!(got.info.vendor.as_deref(), Some("Acme"));
    assert!(slicer::get_profile(&db, 9999).await.unwrap().is_none());

    let found = slicer::find_profile(&db, Kind::Filament, "generic pla")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, f);
    assert!(
        slicer::update_profile(&db, f, Source::User, None, &settings(json!({"x": 1})))
            .await
            .unwrap()
    );
    let got = slicer::get_profile(&db, f).await.unwrap().unwrap();
    assert_eq!(got.info.source, "user");
    assert_eq!(got.info.vendor, None);
    assert_eq!(got.settings["x"], 1);
    let _ = m;
}

#[tokio::test]
async fn printer_settings_check_kinds() {
    let (_app, db) = db().await;
    let pr = printer(&db, "Voron").await;
    let m = profile(&db, Kind::Machine, "M").await;
    let p = profile(&db, Kind::Process, "P").await;
    let f = profile(&db, Kind::Filament, "F").await;
    assert!(slicer::printer_slicer(&db, pr).await.unwrap().is_none());

    let s = PrinterSlicer {
        printer_id: pr,
        machine_profile_id: m,
        default_process_id: Some(p),
        default_filament_id: None,
    };
    assert!(slicer::set_printer_slicer(&db, &s).await.unwrap());
    assert_eq!(slicer::printer_slicer(&db, pr).await.unwrap().unwrap(), s);
    // Update in place.
    let s2 = PrinterSlicer {
        default_filament_id: Some(f),
        ..s.clone()
    };
    assert!(slicer::set_printer_slicer(&db, &s2).await.unwrap());
    assert_eq!(slicer::printer_slicer(&db, pr).await.unwrap().unwrap(), s2);

    // Wrong kinds, missing profiles and missing printers change nothing.
    for bad in [
        PrinterSlicer {
            machine_profile_id: p,
            ..s2.clone()
        },
        PrinterSlicer {
            default_process_id: Some(f),
            ..s2.clone()
        },
        PrinterSlicer {
            default_filament_id: Some(m),
            ..s2.clone()
        },
        PrinterSlicer {
            machine_profile_id: 9999,
            ..s2.clone()
        },
        PrinterSlicer {
            printer_id: 9999,
            ..s2.clone()
        },
    ] {
        assert!(!slicer::set_printer_slicer(&db, &bad).await.unwrap(), "{bad:?}");
    }
    assert_eq!(slicer::printer_slicer(&db, pr).await.unwrap().unwrap(), s2);

    // Deleting the printer removes its settings.
    printers::delete(&db, pr).await.unwrap();
    assert!(slicer::printer_slicer(&db, pr).await.unwrap().is_none());
    assert!(!slicer::clear_printer_slicer(&db, pr).await.unwrap());
}

#[tokio::test]
async fn profiles_in_use_cant_be_deleted() {
    let (_app, db) = db().await;
    let u = user(&db, "op").await;
    let pr = printer(&db, "Voron").await;
    let m = profile(&db, Kind::Machine, "M").await;
    let p = profile(&db, Kind::Process, "P").await;
    let f = profile(&db, Kind::Filament, "F").await;
    let spare = profile(&db, Kind::Filament, "Spare").await;
    slicer::set_printer_slicer(
        &db,
        &PrinterSlicer {
            printer_id: pr,
            machine_profile_id: m,
            default_process_id: Some(p),
            default_filament_id: None,
        },
    )
    .await
    .unwrap();
    let j = slicer::create_job(&db, &job(u, pr, m, p, f)).await.unwrap();

    assert_eq!(
        slicer::delete_profile(&db, m).await.unwrap(),
        DeleteProfile::InUse {
            printers: vec!["Voron".into()],
            jobs: 1
        }
    );
    assert_eq!(
        slicer::delete_profile(&db, p).await.unwrap(),
        DeleteProfile::InUse {
            printers: vec!["Voron".into()],
            jobs: 1
        }
    );
    assert_eq!(
        slicer::delete_profile(&db, f).await.unwrap(),
        DeleteProfile::InUse {
            printers: vec![],
            jobs: 1
        }
    );
    assert_eq!(
        slicer::delete_profile(&db, spare).await.unwrap(),
        DeleteProfile::Deleted
    );
    assert_eq!(
        slicer::delete_profile(&db, spare).await.unwrap(),
        DeleteProfile::NotFound
    );

    // Once the job is finished, its filament can go; the job keeps its row.
    assert!(slicer::cancel(&db, j).await.unwrap());
    assert_eq!(
        slicer::delete_profile(&db, f).await.unwrap(),
        DeleteProfile::Deleted
    );
    let job = slicer::get_job(&db, j).await.unwrap().unwrap();
    assert_eq!(job.filament_id, None);
    assert_eq!(job.process_id, Some(p));
}

#[tokio::test]
async fn job_lifecycle() {
    let (_app, db) = db().await;
    let u = user(&db, "op").await;
    let pr = printer(&db, "Voron").await;
    let (m, p, f) = (
        profile(&db, Kind::Machine, "M").await,
        profile(&db, Kind::Process, "P").await,
        profile(&db, Kind::Filament, "F").await,
    );
    let a = slicer::create_job(&db, &job(u, pr, m, p, f)).await.unwrap();
    let b = slicer::create_job(&db, &job(u, pr, m, p, f)).await.unwrap();
    assert_eq!(slicer::active_jobs_for_user(&db, u).await.unwrap(), 2);
    let ja = slicer::get_job(&db, a).await.unwrap().unwrap();
    assert_eq!(ja.status(), JobStatus::Queued);
    assert_eq!(ja.options_json, r#"{"infill":20}"#);
    assert!(ja.started_at.is_none());

    // Oldest first.
    let claimed = slicer::claim_next(&db).await.unwrap().unwrap();
    assert_eq!(claimed.id, a);
    assert_eq!(claimed.status(), JobStatus::Slicing);
    assert!(claimed.started_at.is_some());
    // A busy worker: back in the queue, still ahead of b.
    assert!(slicer::requeue(&db, a).await.unwrap());
    assert_eq!(slicer::claim_next(&db).await.unwrap().unwrap().id, a);
    assert_eq!(slicer::claim_next(&db).await.unwrap().unwrap().id, b);
    assert!(slicer::claim_next(&db).await.unwrap().is_none());

    // a: sliced, then uploaded.
    let stats = JobStats {
        est_seconds: Some(595),
        filament_mm: Some(1304.61),
        filament_g: Some(3.89),
        layers: Some(100),
        gcode_size: Some(376_370),
    };
    assert!(slicer::sliced(&db, a, &stats).await.unwrap());
    assert!(
        !slicer::sliced(&db, a, &stats).await.unwrap(),
        "only from slicing"
    );
    assert_eq!(
        slicer::get_job(&db, a).await.unwrap().unwrap().status(),
        JobStatus::Uploading
    );
    assert!(slicer::uploaded(&db, a, "printpit/benchy.gcode").await.unwrap());
    let done = slicer::get_job(&db, a).await.unwrap().unwrap();
    assert_eq!(done.status(), JobStatus::Done);
    assert_eq!(done.output_name.as_deref(), Some("printpit/benchy.gcode"));
    assert_eq!(done.est_seconds, Some(595));
    assert_eq!(done.layers, Some(100));
    assert!(done.finished_at.is_some());
    assert!(
        !slicer::cancel(&db, a).await.unwrap(),
        "a finished job can't be cancelled"
    );
    assert!(!slicer::fail(&db, a, "x", None).await.unwrap());

    // b: cancelled mid-slice; the queue's next step then finds nothing to do.
    assert!(slicer::cancel(&db, b).await.unwrap());
    assert!(!slicer::sliced(&db, b, &stats).await.unwrap());
    assert!(!slicer::requeue(&db, b).await.unwrap());
    assert_eq!(
        slicer::get_job(&db, b).await.unwrap().unwrap().status(),
        JobStatus::Cancelled
    );
    assert_eq!(slicer::active_jobs_for_user(&db, u).await.unwrap(), 0);

    // Reslice a finished job: back in the queue, old results cleared.
    assert!(slicer::retry(&db, a, r#"{"infill":40}"#, true).await.unwrap());
    let again = slicer::get_job(&db, a).await.unwrap().unwrap();
    assert_eq!(again.status(), JobStatus::Queued);
    assert_eq!(again.options_json, r#"{"infill":40}"#);
    assert!(again.start_print);
    assert!(again.output_name.is_none() && again.est_seconds.is_none() && again.finished_at.is_none());
    assert!(
        !slicer::retry(&db, a, "{}", false).await.unwrap(),
        "not while queued"
    );

    // Failing keeps the slicer's log.
    slicer::claim_next(&db).await.unwrap();
    assert!(
        slicer::fail(&db, a, "bad model", Some("line 1\nline 2"))
            .await
            .unwrap()
    );
    let failed = slicer::get_job(&db, a).await.unwrap().unwrap();
    assert_eq!(failed.status(), JobStatus::Failed);
    assert_eq!(failed.error.as_deref(), Some("bad model"));
    assert_eq!(failed.log_tail.as_deref(), Some("line 1\nline 2"));
    // A new attempt starts clean.
    slicer::retry(&db, a, "{}", false).await.unwrap();
    let claimed = slicer::claim_next(&db).await.unwrap().unwrap();
    assert!(claimed.error.is_none() && claimed.log_tail.is_none());
}

#[tokio::test]
async fn restart_interrupts_running_jobs_and_keeps_the_queue() {
    let (_app, db) = db().await;
    let u = user(&db, "op").await;
    let pr = printer(&db, "Voron").await;
    let (m, p, f) = (
        profile(&db, Kind::Machine, "M").await,
        profile(&db, Kind::Process, "P").await,
        profile(&db, Kind::Filament, "F").await,
    );
    let slicing = slicer::create_job(&db, &job(u, pr, m, p, f)).await.unwrap();
    let uploading = slicer::create_job(&db, &job(u, pr, m, p, f)).await.unwrap();
    let queued = slicer::create_job(&db, &job(u, pr, m, p, f)).await.unwrap();
    slicer::claim_next(&db).await.unwrap();
    slicer::claim_next(&db).await.unwrap();
    slicer::sliced(&db, uploading, &JobStats::default())
        .await
        .unwrap();

    assert_eq!(slicer::interrupt_running(&db).await.unwrap(), 2);
    for id in [slicing, uploading] {
        let j = slicer::get_job(&db, id).await.unwrap().unwrap();
        assert_eq!(j.status(), JobStatus::Failed);
        assert_eq!(j.error.as_deref(), Some(slicer::INTERRUPTED));
    }
    assert_eq!(
        slicer::get_job(&db, queued).await.unwrap().unwrap().status(),
        JobStatus::Queued
    );
    assert_eq!(slicer::interrupt_running(&db).await.unwrap(), 0);
}

#[tokio::test]
async fn deleting_and_retention() {
    let (_app, db) = db().await;
    let u = user(&db, "op").await;
    let pr = printer(&db, "Voron").await;
    let (m, p, f) = (
        profile(&db, Kind::Machine, "M").await,
        profile(&db, Kind::Process, "P").await,
        profile(&db, Kind::Filament, "F").await,
    );
    let active = slicer::create_job(&db, &job(u, pr, m, p, f)).await.unwrap();
    let old = slicer::create_job(&db, &job(u, pr, m, p, f)).await.unwrap();
    slicer::cancel(&db, old).await.unwrap();

    assert!(
        !slicer::delete_job(&db, active).await.unwrap(),
        "unfinished jobs aren't deleted"
    );
    let now = printpit::db::now();
    // Everything finished just now is kept with a cutoff in the past...
    assert_eq!(
        slicer::retained_job_ids(&db, now - 60).await.unwrap(),
        [active, old]
    );
    // ...and only unfinished jobs with a cutoff in the future.
    assert_eq!(slicer::retained_job_ids(&db, now + 60).await.unwrap(), [active]);

    assert!(slicer::delete_job(&db, old).await.unwrap());
    assert!(slicer::get_job(&db, old).await.unwrap().is_none());

    // Recent jobs, newest first, per printer.
    let other = printer(&db, "Prusa").await;
    let third = slicer::create_job(&db, &job(u, other, m, p, f)).await.unwrap();
    let all: Vec<i64> = slicer::recent_jobs(&db, None, 10)
        .await
        .unwrap()
        .iter()
        .map(|j| j.id)
        .collect();
    assert_eq!(all, [third, active]);
    let voron: Vec<i64> = slicer::recent_jobs(&db, Some(pr), 10)
        .await
        .unwrap()
        .iter()
        .map(|j| j.id)
        .collect();
    assert_eq!(voron, [active]);
    assert_eq!(slicer::recent_jobs(&db, None, 1).await.unwrap().len(), 1);

    // A deleted user or printer leaves the job (and its history) in place.
    users::delete(&db, u).await.unwrap();
    printers::delete(&db, other).await.unwrap();
    let j = slicer::get_job(&db, third).await.unwrap().unwrap();
    assert_eq!((j.user_id, j.printer_id), (None, None));
}

//! Acceptance test of the jobs of Loupe (F15, space).
//!
//! The space screen runs each walk as a background job. A job runs one
//! command (`space`, `pathSpace` or `reclaimable`) and has the id that the
//! window gives it. `Jobs::start` starts a job, and stops the older job of
//! the same command at its next step: a click in the treemap starts a new
//! path walk, and the window no longer needs the walk of the path before
//! it. A job of another command goes on. `Jobs::stop_all` stops every job,
//! as the close of a folder does.
//!
//! The progress event of a job names its command and its id, so the window
//! shows only the progress of the job that it waits for:
//! `{"job": "pathSpace", "id": 7, "walk": "objects", "done": 3}`.
//!
//! The window side, `apps/loupe/tests/e2e/space_jobs.spec.ts`, checks that
//! the IPC commands take the id and put it in their events. The stop is
//! checked here, because a walk of the fixture store ends before a second
//! call can reach it.

mod common;

use std::ops::ControlFlow;

use zoe_loupe::{Error, Folder, Job, Jobs, Progress, Walk};

use common::{fresh_dir, write_store};

/// The progress function of a walk that runs as `job`: it stops the walk
/// once the job is stopped.
fn step(job: &Job) -> ControlFlow<()> {
    if job.is_stopped() {
        ControlFlow::Break(())
    } else {
        ControlFlow::Continue(())
    }
}

#[test]
fn a_new_job_stops_the_older_job_of_its_command_only() {
    let jobs = Jobs::default();
    let first = jobs.start("pathSpace", 1);
    let space = jobs.start("space", 2);
    assert!(!first.is_stopped());

    let second = jobs.start("pathSpace", 3);
    assert!(first.is_stopped());
    assert!(!second.is_stopped());
    assert!(!space.is_stopped(), "a job of another command stopped");

    // The close of the folder stops every job.
    jobs.stop_all();
    assert!(second.is_stopped());
    assert!(space.is_stopped());
}

#[test]
fn a_path_walk_stops_when_the_window_starts_another_one() {
    let dir = fresh_dir("jobs-walk");
    write_store(&dir);
    let folder = Folder::open(&dir).unwrap();
    let whole = folder
        .path_space("/plone", |_| ControlFlow::Continue(()))
        .unwrap();

    let jobs = Jobs::default();
    let first = jobs.start("pathSpace", 1);
    let mut second = None;
    let stopped = folder.path_space("/plone", |_| {
        // A click in the treemap starts a new path walk during this one.
        if second.is_none() {
            second = Some(jobs.start("pathSpace", 2));
        }
        step(&first)
    });
    assert!(matches!(stopped, Err(Error::Stopped)), "{stopped:?}");

    // The new walk runs to its end and gives what a walk alone gives.
    let second = second.expect("the first walk made no step");
    let walked = folder.path_space("/plone", |_| step(&second)).unwrap();
    assert_eq!(walked, whole);
}

#[test]
fn the_progress_event_names_the_command_and_the_id_of_its_job() {
    let jobs = Jobs::default();
    let job = jobs.start("pathSpace", 7);
    let event = job.event(Progress {
        walk: Walk::Objects,
        done: 3,
    });
    assert_eq!(
        serde_json::to_value(event).unwrap(),
        serde_json::json!({"job": "pathSpace", "id": 7, "walk": "objects", "done": 3})
    );
}

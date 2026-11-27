//! Questions to the user about requests a [`GuardRule`] in `Ask` mode
//! holds back. The proxy thread that serves the request waits in
//! [`Approvals::ask`]; the app lists [`Approvals::pending`] and answers with
//! [`Approvals::answer`].
//!
//! One question covers a guard, a host, a scope and a subject: a second
//! request for the same four waits for the same answer, and the answer
//! holds for [`REMEMBER`], so a `git push` (one `GET`, then a `POST`) asks
//! once. The subject is what the request acts on, as far as its target and
//! body tell: the path without its query, cut before the part that names
//! the action (`/info/refs`, `/git-receive-pack`, `/blobs/`, `/manifests/`),
//! and the body a guard read. A push to another repository, or a second
//! mutation with other variables, asks again, so the question the user
//! answered is the one that passes. A question nobody answers within
//! [`TIMEOUT`] is refused.
//!
//! The scope names where a request comes from, such as a project, when the
//! proxy or the keyring bus knows it. Within a scope a guard and host can
//! be allowed for good ([`Approvals::set_always`]); the app keeps that list
//! and hands it over. A guard and host can also be refused for good
//! ([`Approvals::set_blocked`]), which wins over every other answer and
//! never asks. A guard and host can also be allowed until the app
//! quits ([`Approvals::allow_for_session`]), within the question's scope.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::rules::{GuardRule, normalize_target};

/// How long an answer covers further requests for the same guard and host.
pub const REMEMBER: Duration = Duration::from_secs(60);
/// How long a request waits for an answer before it is refused.
pub const TIMEOUT: Duration = Duration::from_secs(120);

/// Ends the cover of the answers given in `scope`. Requests still waiting
/// for one of them read it by id, so the answers themselves stay.
fn forget_answers(state: &mut State, scope: &str) {
    for answer in &mut state.answers {
        if answer.scope.as_deref() == Some(scope) {
            answer.reuse = false;
        }
    }
}

/// A request waiting for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    pub id: u64,
    /// [`GuardRule::id`].
    pub guard: String,
    /// [`GuardRule::title`].
    pub title: String,
    /// Where the request comes from, if the proxy knows it. Only a scoped
    /// question can be allowed for good.
    pub scope: Option<String>,
    pub host: String,
    pub method: String,
    pub target: String,
    /// What the request sends, such as the start of a GraphQL mutation,
    /// when a guard read its body.
    pub detail: Option<String>,
    /// What the request acts on, see the module docs.
    subject: String,
}

impl Approval {
    fn is(&self, guard: &str, scope: Option<&str>, host: &str) -> bool {
        self.guard == guard && self.scope.as_deref() == scope && self.host == host
    }
}

/// What a request to `target` acts on, see the module docs. `body` is the
/// whole body a guard read, of which the question shows only a part, such
/// as a GraphQL request's variables without its query.
fn subject(target: &str, detail: Option<&str>, body: Option<&[u8]>) -> String {
    let mut path = normalize_target(target);
    if let Some(at) = path.find('?') {
        path.truncate(at);
    }
    let cut = ["/info/refs", "/git-receive-pack", "/blobs/", "/manifests/"]
        .iter()
        .filter_map(|action| path.find(action))
        .min();
    if let Some(at) = cut {
        path.truncate(at);
    }
    if let Some(body) = body {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        body.hash(&mut hasher);
        path.push_str(&format!(" body {:016x}", hasher.finish()));
    } else if let Some(detail) = detail {
        path.push(' ');
        path.push_str(detail);
    }
    path
}

struct Answer {
    id: u64,
    guard: String,
    scope: Option<String>,
    host: String,
    subject: String,
    allow: bool,
    at: Instant,
    /// Whether the answer still covers later requests within [`REMEMBER`].
    /// A change to the standing answers of its scope ends that, but the
    /// answer stays for the requests still waiting for it.
    reuse: bool,
}

#[derive(Default)]
struct State {
    next: u64,
    pending: Vec<Approval>,
    answers: Vec<Answer>,
    /// Per scope, the guards and hosts allowed for good.
    always: HashMap<String, Vec<(String, String)>>,
    /// Per scope, the guards and hosts refused for good. They win over
    /// every other answer.
    blocked: HashMap<String, Vec<(String, String)>>,
    /// `(scope, guard, host)` allowed until the app quits.
    session: Vec<(Option<String>, String, String)>,
}

/// The questions of one proxy.
pub struct Approvals {
    state: Mutex<State>,
    answered: Condvar,
    revision: AtomicU64,
    notify: Box<dyn Fn() + Send + Sync>,
}

impl Approvals {
    /// `notify` runs on a proxy thread whenever [`Self::pending`] changes.
    pub fn new(notify: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            state: Mutex::default(),
            answered: Condvar::new(),
            revision: AtomicU64::new(0),
            notify: Box::new(notify),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn changed(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
        (self.notify)();
    }

    /// Asks whether `method target` to `host` may pass `guard`, for a
    /// request from `scope`, and blocks until the user answers or
    /// [`TIMEOUT`] runs out. `host` is compared as given, so callers pass
    /// host names in lowercase. The keyring bus (`keyring.rs`) passes a
    /// secret's label instead.
    pub fn ask(
        &self,
        guard: &GuardRule,
        scope: Option<&str>,
        host: &str,
        method: &str,
        target: &str,
    ) -> bool {
        self.ask_within(guard, scope, host, method, target, None, None, TIMEOUT)
    }

    /// [`Self::ask`] for a request whose body the guard read: `detail`
    /// is what the question shows of it, and `body` the whole of it. An
    /// answer covers only requests with the same body.
    #[allow(clippy::too_many_arguments)]
    pub fn ask_about(
        &self,
        guard: &GuardRule,
        scope: Option<&str>,
        host: &str,
        method: &str,
        target: &str,
        detail: Option<String>,
        body: Option<&[u8]>,
    ) -> bool {
        self.ask_within(guard, scope, host, method, target, detail, body, TIMEOUT)
    }

    /// [`Self::ask`] with `detail` shown in the question but left out of
    /// what an answer covers: within [`REMEMBER`] it covers requests with
    /// other details too, such as the commits of a rebase that git signs
    /// one by one, each with its own subject.
    pub fn ask_noting(
        &self,
        guard: &GuardRule,
        scope: Option<&str>,
        host: &str,
        method: &str,
        target: &str,
        detail: Option<String>,
    ) -> bool {
        let about = subject(target, None, None);
        self.ask_subject(guard, scope, host, method, target, detail, about, TIMEOUT)
    }

    #[allow(clippy::too_many_arguments)]
    fn ask_within(
        &self,
        guard: &GuardRule,
        scope: Option<&str>,
        host: &str,
        method: &str,
        target: &str,
        detail: Option<String>,
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> bool {
        let about = subject(target, detail.as_deref(), body);
        self.ask_subject(guard, scope, host, method, target, detail, about, timeout)
    }

    /// Asks about `about`, the subject of the request (see [`subject`]).
    #[allow(clippy::too_many_arguments)]
    fn ask_subject(
        &self,
        guard: &GuardRule,
        scope: Option<&str>,
        host: &str,
        method: &str,
        target: &str,
        detail: Option<String>,
        about: String,
        timeout: Duration,
    ) -> bool {
        let mut state = self.lock();
        if let Some(blocked) = scope.and_then(|scope| state.blocked.get(scope))
            && blocked.iter().any(|(g, h)| *g == guard.id && h == host)
        {
            return false;
        }
        if let Some(always) = scope.and_then(|scope| state.always.get(scope))
            && always.iter().any(|(g, h)| *g == guard.id && h == host)
        {
            return true;
        }
        if (state.session.iter())
            .any(|(s, g, h)| s.as_deref() == scope && *g == guard.id && h == host)
        {
            return true;
        }
        let now = Instant::now();
        state
            .answers
            .retain(|a| now.duration_since(a.at) < REMEMBER);
        if let Some(answer) = (state.answers.iter()).find(|a| {
            a.reuse
                && a.guard == guard.id
                && a.scope.as_deref() == scope
                && a.host == host
                && a.subject == about
        }) {
            return answer.allow;
        }
        let id = match (state.pending.iter())
            .find(|p| p.is(&guard.id, scope, host) && p.subject == about)
        {
            Some(pending) => pending.id,
            None => {
                state.next += 1;
                let id = state.next;
                state.pending.push(Approval {
                    id,
                    guard: guard.id.clone(),
                    title: guard.title.clone(),
                    scope: scope.map(str::to_owned),
                    host: host.to_owned(),
                    method: method.to_owned(),
                    target: target.to_owned(),
                    detail,
                    subject: about,
                });
                drop(state);
                self.changed();
                state = self.lock();
                id
            }
        };
        let deadline = now + timeout;
        loop {
            if let Some(answer) = state.answers.iter().find(|a| a.id == id) {
                return answer.allow;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(state);
                // Other requests waiting on the same question get the refusal too.
                self.answer(id, false);
                return false;
            }
            state = self
                .answered
                .wait_timeout(state, left)
                .unwrap_or_else(|poison| poison.into_inner())
                .0;
        }
    }

    /// The questions waiting for an answer, oldest first.
    pub fn pending(&self) -> Vec<Approval> {
        self.lock().pending.clone()
    }

    /// Changes whenever [`Self::pending`] changes.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    /// Answers question `id`. Unknown ids are ignored.
    pub fn answer(&self, id: u64, allow: bool) {
        let mut state = self.lock();
        let Some(index) = state.pending.iter().position(|p| p.id == id) else {
            return;
        };
        let pending = state.pending.remove(index);
        state.answers.push(Answer {
            id,
            subject: pending.subject.clone(),
            guard: pending.guard,
            scope: pending.scope,
            host: pending.host,
            allow,
            at: Instant::now(),
            reuse: true,
        });
        drop(state);
        self.answered.notify_all();
        self.changed();
    }

    /// Allows question `id`, and its guard and host in its scope until the
    /// app quits. Waiting questions it covers are allowed too.
    pub fn allow_for_session(&self, id: u64) {
        let mut state = self.lock();
        let Some(question) = state.pending.iter().find(|p| p.id == id).cloned() else {
            return;
        };
        let entry = (
            question.scope.clone(),
            question.guard.clone(),
            question.host.clone(),
        );
        if !state.session.contains(&entry) {
            state.session.push(entry);
        }
        let covered: Vec<u64> = (state.pending.iter())
            .filter(|p| p.is(&question.guard, question.scope.as_deref(), &question.host))
            .map(|p| p.id)
            .collect();
        drop(state);
        for id in covered {
            self.answer(id, true);
        }
    }

    /// Replaces the guards and hosts allowed for good in `scope`, as
    /// `(guard id, host)`. Waiting questions they cover are allowed.
    pub fn set_always(&self, scope: &str, always: Vec<(String, String)>) {
        let mut state = self.lock();
        if state.always.get(scope).map_or(&[][..], Vec::as_slice) == always.as_slice() {
            return;
        }
        let covered: Vec<u64> = (state.pending.iter())
            .filter(|p| p.scope.as_deref() == Some(scope))
            .filter(|p| always.iter().any(|(g, h)| *g == p.guard && *h == p.host))
            .map(|p| p.id)
            .collect();
        if always.is_empty() {
            state.always.remove(scope);
        } else {
            state.always.insert(scope.to_owned(), always);
        }
        // A revoked guard asks again, also within the remembered minute.
        forget_answers(&mut state, scope);
        drop(state);
        for id in covered {
            self.answer(id, true);
        }
    }

    /// Replaces the guards and hosts refused for good in `scope`, as
    /// `(guard id, host)`. Waiting questions they cover are refused.
    pub fn set_blocked(&self, scope: &str, blocked: Vec<(String, String)>) {
        let mut state = self.lock();
        if state.blocked.get(scope).map_or(&[][..], Vec::as_slice) == blocked.as_slice() {
            return;
        }
        let covered: Vec<u64> = (state.pending.iter())
            .filter(|p| p.scope.as_deref() == Some(scope))
            .filter(|p| blocked.iter().any(|(g, h)| *g == p.guard && *h == p.host))
            .map(|p| p.id)
            .collect();
        if blocked.is_empty() {
            state.blocked.remove(scope);
        } else {
            state.blocked.insert(scope.to_owned(), blocked);
        }
        // An unblocked guard asks again, also within the remembered minute.
        forget_answers(&mut state, scope);
        drop(state);
        for id in covered {
            self.answer(id, false);
        }
    }

    /// Refuses every waiting question, such as when the app quits.
    pub fn refuse_all(&self) {
        let ids: Vec<u64> = self.lock().pending.iter().map(|p| p.id).collect();
        for id in ids {
            self.answer(id, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    use super::*;
    use crate::proxy::rules::GuardMode;

    fn guard() -> GuardRule {
        GuardRule {
            id: "git.push".into(),
            title: "git push".into(),
            hosts: vec!["*".into()],
            requests: vec!["POST */git-receive-pack".into()],
            mode: GuardMode::Ask,
            kind: None,
        }
    }

    #[test]
    fn one_answer_covers_waiting_and_later_requests_for_the_host() {
        let notified = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&notified);
        let approvals = Arc::new(Approvals::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        }));
        let ask = |host: &'static str| {
            let approvals = Arc::clone(&approvals);
            thread::spawn(move || {
                approvals.ask(&guard(), None, host, "POST", "/r/git-receive-pack")
            })
        };
        let first = ask("example.org");
        let second = ask("example.org");
        while approvals.pending().is_empty() {
            thread::yield_now();
        }
        // Both requests share one question.
        thread::sleep(Duration::from_millis(50));
        let pending = approvals.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].host, "example.org");
        let revision = approvals.revision();
        approvals.answer(pending[0].id, true);
        assert!(first.join().unwrap());
        assert!(second.join().unwrap());
        assert_ne!(approvals.revision(), revision);
        assert!(notified.load(Ordering::Relaxed) >= 2);
        // Remembered: no new question.
        assert!(approvals.ask(
            &guard(),
            None,
            "example.org",
            "GET",
            "/r/info/refs?service=x"
        ));
        assert!(approvals.pending().is_empty());
    }

    #[test]
    fn an_answer_covers_only_what_the_question_showed() {
        let approvals = Approvals::new(|| {});
        let short = Duration::from_millis(20);
        // `detail` is what the question shows, `body` the whole request.
        let push_body = |target: &str, detail: Option<&str>, body: Option<&str>| {
            approvals.ask_within(
                &guard(),
                None,
                "example.org",
                "POST",
                target,
                detail.map(str::to_owned),
                body.map(str::as_bytes),
                short,
            )
        };
        let push = |target: &str, detail: Option<&str>| push_body(target, detail, detail);
        assert!(!push("/me/scratch.git/git-receive-pack", None));
        // The refusal is remembered for that repository only.
        let other = thread::scope(|scope| {
            let asking = scope.spawn(|| push("/org/prod.git/git-receive-pack", None));
            while approvals.pending().is_empty() {
                thread::yield_now();
            }
            let pending = approvals.pending();
            assert_eq!(pending[0].target, "/org/prod.git/git-receive-pack");
            approvals.answer(pending[0].id, true);
            asking.join().unwrap()
        });
        assert!(other);
        assert!(!push("/me/scratch.git/git-receive-pack", None));
        assert!(push("/ORG/prod.git//info/refs", None));
        // A body the guard read is part of what was answered.
        let allowed = thread::scope(|scope| {
            let asking = scope.spawn(|| push("/graphql", Some("{\"title\":\"a\"}")));
            while approvals.pending().is_empty() {
                thread::yield_now();
            }
            approvals.answer(approvals.pending()[0].id, true);
            asking.join().unwrap()
        });
        assert!(allowed);
        assert!(push("/graphql", Some("{\"title\":\"a\"}")));
        assert!(!push("/graphql", Some("{\"title\":\"b\"}")));
        // The same variables in another mutation are another request.
        let other = "mutation{merge} {\"title\":\"a\"}";
        assert!(!push_body(
            "/graphql",
            Some("{\"title\":\"a\"}"),
            Some(other)
        ));
    }

    #[test]
    fn unanswered_questions_are_refused() {
        let approvals = Approvals::new(|| {});
        let refused = !approvals.ask_within(
            &guard(),
            None,
            "a.org",
            "POST",
            "/",
            None,
            None,
            Duration::from_millis(20),
        );
        assert!(refused);
        assert!(approvals.pending().is_empty());
    }

    #[test]
    fn standing_answers_hold_within_their_scope_only() {
        let approvals = Approvals::new(|| {});
        let short = Duration::from_millis(20);
        let always = vec![("git.push".to_owned(), "example.org".to_owned())];
        approvals.set_always("1", always);
        assert!(approvals.ask_within(
            &guard(),
            Some("1"),
            "example.org",
            "POST",
            "/",
            None,
            None,
            short
        ));
        // Another project, another host, or no scope still asks.
        assert!(!approvals.ask_within(
            &guard(),
            Some("2"),
            "example.org",
            "POST",
            "/",
            None,
            None,
            short
        ));
        assert!(!approvals.ask_within(
            &guard(),
            Some("1"),
            "other.org",
            "POST",
            "/",
            None,
            None,
            short
        ));
        assert!(!approvals.ask_within(
            &guard(),
            None,
            "example.org",
            "POST",
            "/",
            None,
            None,
            short
        ));
        // Revoked, it asks again.
        approvals.set_always("1", Vec::new());
        assert!(!approvals.ask_within(
            &guard(),
            Some("1"),
            "example.org",
            "POST",
            "/",
            None,
            None,
            short
        ));
    }

    #[test]
    fn session_answers_hold_for_their_scope_guard_and_host() {
        let approvals = Arc::new(Approvals::new(|| {}));
        let waiting = {
            let approvals = Arc::clone(&approvals);
            thread::spawn(move || {
                let detail = Some("mutation{createPullRequest}".to_owned());
                approvals.ask_about(
                    &guard(),
                    Some("3"),
                    "example.org",
                    "POST",
                    "/graphql",
                    detail,
                    None,
                )
            })
        };
        while approvals.pending().is_empty() {
            thread::yield_now();
        }
        let question = approvals.pending()[0].clone();
        assert_eq!(
            question.detail.as_deref(),
            Some("mutation{createPullRequest}")
        );
        approvals.allow_for_session(question.id);
        assert!(waiting.join().unwrap());
        let short = Duration::from_millis(20);
        // Past the remembered minute it still holds; elsewhere it asks.
        approvals.lock().answers.clear();
        assert!(approvals.ask_within(
            &guard(),
            Some("3"),
            "example.org",
            "POST",
            "/",
            None,
            None,
            short
        ));
        assert!(!approvals.ask_within(
            &guard(),
            Some("4"),
            "example.org",
            "POST",
            "/",
            None,
            None,
            short
        ));
        assert!(!approvals.ask_within(
            &guard(),
            Some("3"),
            "other.org",
            "POST",
            "/",
            None,
            None,
            short
        ));
    }

    #[test]
    fn allowing_for_good_answers_the_waiting_question() {
        let approvals = Arc::new(Approvals::new(|| {}));
        let waiting = {
            let approvals = Arc::clone(&approvals);
            thread::spawn(move || approvals.ask(&guard(), Some("7"), "example.org", "POST", "/"))
        };
        while approvals.pending().is_empty() {
            thread::yield_now();
        }
        assert_eq!(approvals.pending()[0].scope.as_deref(), Some("7"));
        approvals.set_always("7", vec![("git.push".into(), "example.org".into())]);
        assert!(waiting.join().unwrap());
    }

    #[test]
    fn blocked_answers_refuse_without_asking_and_win_over_allowed_ones() {
        let approvals = Arc::new(Approvals::new(|| {}));
        let waiting = {
            let approvals = Arc::clone(&approvals);
            thread::spawn(move || approvals.ask(&guard(), Some("5"), "example.org", "POST", "/"))
        };
        while approvals.pending().is_empty() {
            thread::yield_now();
        }
        let entry = vec![("git.push".to_owned(), "example.org".to_owned())];
        approvals.set_blocked("5", entry.clone());
        assert!(!waiting.join().unwrap());
        // No question waits for it, even when the same entry is allowed too.
        approvals.set_always("5", entry);
        assert!(!approvals.ask(&guard(), Some("5"), "example.org", "POST", "/"));
        assert!(approvals.pending().is_empty());
        // Another project still asks; unblocked, the allowed entry holds.
        let short = Duration::from_millis(20);
        assert!(!approvals.ask_within(
            &guard(),
            Some("6"),
            "example.org",
            "POST",
            "/",
            None,
            None,
            short
        ));
        approvals.set_blocked("5", Vec::new());
        assert!(approvals.ask(&guard(), Some("5"), "example.org", "POST", "/"));
    }
}

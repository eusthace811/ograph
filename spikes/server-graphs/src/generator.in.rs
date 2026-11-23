const OPS: &str = "http://ops.example/";
const START_MS: u64 = 1_790_812_800_000;
const DAY_MS: u64 = 86_400_000;
const WINDOW_MS: u64 = 600_000;
const N_APPS: u64 = 20_000;
const N_INCIDENTS: u64 = 100_000;
const N_EVENTS: u64 = 1_000_000;
const RUNS: usize = 7;
const TEAMS: [&str; 10] = [
    "Bedrock", "Atlas", "Keystone", "Nimbus", "Orion", "Pulse", "Quarry", "Relay", "Summit", "Tundra",
];
const REGIONS: [&str; 5] = ["us-east-1", "us-west-2", "eu-west-1", "ap-south-1", "sa-east-1"];
const EVENT_TYPES: [&str; 5] = ["Deployment", "LogError", "Alert", "ConfigChange", "MetricAnomaly"];
const PREFIX: &str =
    "PREFIX ops: <http://ops.example/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> ";

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn ops(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{OPS}{local}"))
}

fn date_time(ms: u64) -> Literal {
    let (day, r) = ((ms - START_MS) / DAY_MS, (ms - START_MS) % DAY_MS);
    let (h, m, s, milli) = (r / 3_600_000, r / 60_000 % 60, r / 1000 % 60, r % 1000);
    let lexical = format!("2026-10-{:02}T{h:02}:{m:02}:{s:02}.{milli:03}Z", day + 1);
    Literal::new_typed_literal(lexical, xsd::DATE_TIME)
}

struct Incident {
    iri: NamedNode,
    precursors: u64,
}

struct World {
    quads: Vec<Quad>,
    incidents: Vec<Incident>,
    apps: Vec<NamedNode>,
    dependents: Vec<u32>,
}

fn generate() -> World {
    let mut rng = Rng(42);
    let mut quads = Vec::with_capacity(4_000_000);
    let mut add = |s: &NamedNode, p: &str, o: Term| {
        let p = if p == "a" { rdf::TYPE.into_owned() } else { ops(p) };
        quads.push(Quad::new(s.clone(), p, o, GraphName::DefaultGraph));
    };
    let apps: Vec<NamedNode> = (0..N_APPS).map(|i| ops(&format!("app/{i}"))).collect();
    let mut deps = vec![Vec::new(); apps.len()];
    let mut dependents = vec![0u32; apps.len()];
    for (i, app) in apps.iter().enumerate() {
        add(app, "a", ops("Application").into());
        add(app, "ownedBy", Literal::from(TEAMS[rng.below(10) as usize]).into());
        add(app, "deployedIn", Literal::from(REGIONS[rng.below(5) as usize]).into());
        let k = (1 + rng.below(5) as usize).min(i);
        while deps[i].len() < k {
            let j = (i as f64 * rng.unit().powi(2)) as usize;
            if !deps[i].contains(&j) {
                deps[i].push(j);
                dependents[j] += 1;
                add(app, "dependsOn", apps[j].clone().into());
            }
        }
    }
    let mut n_events = 0u64;
    let event = |add: &mut dyn FnMut(&NamedNode, &str, Term), rng: &mut Rng, n: u64, app: usize, ms| {
        let e = ops(&format!("ev/{n}"));
        add(&e, "a", ops(EVENT_TYPES[rng.below(5) as usize]).into());
        add(&e, "about", apps[app].clone().into());
        add(&e, "at", date_time(ms).into());
    };
    let mut incidents = Vec::new();
    for j in 0..N_INCIDENTS {
        let iri = ops(&format!("inc/{j}"));
        let a = rng.below(N_APPS) as usize;
        let ms = START_MS + 3_600_000 + rng.below(30 * DAY_MS - 3_600_000);
        add(&iri, "a", ops("Incident").into());
        add(&iri, "affects", apps[a].clone().into());
        add(&iri, "severity", Literal::from(format!("SEV{}", 1 + rng.below(3))).into());
        add(&iri, "openedAt", date_time(ms).into());
        let precursors = rng.below(6);
        for _ in 0..precursors {
            let target = match deps[a].len() {
                0 => a,
                n if rng.below(2) == 0 => deps[a][rng.below(n as u64) as usize],
                _ => a,
            };
            let at = ms - rng.below(12 * 60_000);
            event(&mut add, &mut rng, n_events, target, at);
            n_events += 1;
        }
        incidents.push(Incident { iri, precursors });
    }
    for n in n_events..N_EVENTS {
        let (app, at) = (rng.below(N_APPS) as usize, START_MS + rng.below(30 * DAY_MS));
        event(&mut add, &mut rng, n, app, at);
    }
    World { quads, incidents, apps, dependents }
}
struct Native<'a> {
    t: &'a Tables,
}

impl Native<'_> {
    fn id(&self, term: impl Into<Term>) -> u64 {
        self.t.id(&term.into()).unwrap().expect("term in dictionary")
    }
    fn lookup(&self, spo: [Option<u64>; 3]) -> Vec<u64> {
        self.t.lookup(spo).unwrap()
    }
    fn terms(&self, rows: Vec<Vec<u64>>) -> Rows {
        rows.into_iter().map(|r| r.into_iter().map(|id| self.t.term(id).unwrap()).collect()).collect()
    }

    fn incidents_of_bedrock_us_east(&self) -> Rows {
        let (owned, region) = (self.id(ops("ownedBy")), self.id(ops("deployedIn")));
        let team: HashSet<u64> = self.lookup([None, Some(owned), Some(self.id(Literal::from("Bedrock")))]).into_iter().collect();
        let apps = self.lookup([None, Some(region), Some(self.id(Literal::from("us-east-1")))]);
        let (affects, a, incident) = (self.id(ops("affects")), self.id(rdf::TYPE), self.id(ops("Incident")));
        let mut rows = Vec::new();
        for app in apps.into_iter().filter(|x| team.contains(x)) {
            for inc in self.lookup([None, Some(affects), Some(app)]) {
                if self.t.quads([Some(inc), Some(a), Some(incident)], Some(None)).next().is_some() {
                    rows.push(vec![inc, app]);
                }
            }
        }
        let mut rows = self.terms(rows);
        rows.sort_by_cached_key(|r| r[0].to_string());
        rows
    }

    fn correlated_events(&self, incident: &NamedNode) -> Rows {
        let inc = self.id(incident.clone());
        let app = self.lookup([Some(inc), Some(self.id(ops("affects"))), None])[0];
        let opened = self.lookup([Some(inc), Some(self.id(ops("openedAt"))), None])[0];
        let t0 = store::date_time_millis(&self.t.term(opened).unwrap()).unwrap();
        let mut scope: HashSet<u64> = self.lookup([Some(app), Some(self.id(ops("dependsOn"))), None]).into_iter().collect();
        scope.insert(app);
        let about = self.id(ops("about"));
        let events = self.t.events_between(t0 - WINDOW_MS, t0).unwrap();
        let hits = events.into_iter().filter(|&e| self.lookup([Some(e), Some(about), None]).iter().any(|x| scope.contains(x)));
        self.terms(hits.map(|e| vec![e]).collect())
    }

    /// (events about the apps in scope over all time, events in the window): the work each path scans.
    fn w2_work(&self, incident: &NamedNode) -> (usize, usize) {
        let inc = self.id(incident.clone());
        let app = self.lookup([Some(inc), Some(self.id(ops("affects"))), None])[0];
        let mut scope = self.lookup([Some(app), Some(self.id(ops("dependsOn"))), None]);
        scope.push(app);
        let about = self.id(ops("about"));
        let history = scope.iter().map(|&x| self.lookup([None, Some(about), Some(x)]).len()).sum();
        let opened = self.lookup([Some(inc), Some(self.id(ops("openedAt"))), None])[0];
        let t0 = store::date_time_millis(&self.t.term(opened).unwrap()).unwrap();
        (history, self.t.events_between(t0 - WINDOW_MS, t0).unwrap().len())
    }

    fn blast_radius(&self, app: &NamedNode) -> Rows {
        let depends_on = self.id(ops("dependsOn"));
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([self.id(app.clone())]);
        while let Some(node) = queue.pop_front() {
            for x in self.lookup([None, Some(depends_on), Some(node)]) {
                if seen.insert(x) {
                    queue.push_back(x);
                }
            }
        }
        self.terms(seen.into_iter().map(|x| vec![x]).collect())
    }
}
/// 20 incidents with at least one precursor, and 20 start apps: the hub plus 19 with a dependent.
fn instances(world: &World) -> (Vec<&Incident>, Vec<usize>) {
    let mut rng = Rng(7);
    let mut picked: Vec<&Incident> = Vec::new();
    while picked.len() < 20 {
        let inc = &world.incidents[rng.below(N_INCIDENTS) as usize];
        if inc.precursors > 0 && !picked.iter().any(|p| p.iri == inc.iri) {
            picked.push(inc);
        }
    }
    let hub = (0..world.apps.len()).max_by_key(|&i| world.dependents[i]).unwrap();
    let mut starts = vec![hub];
    while starts.len() < 20 {
        let i = rng.below(N_APPS) as usize;
        if world.dependents[i] > 0 && !starts.contains(&i) {
            starts.push(i);
        }
    }
    (picked, starts)
}

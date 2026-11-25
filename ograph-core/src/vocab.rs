//! IRIs of the ograph ontology (`ontology/ograph.ttl`). Code refers to the ontology only through
//! these constants, and `ontology check` fails when one is missing from the Turtle file.

macro_rules! ns {
    ($module:ident, $base:literal, { $($name:ident = $local:literal),* $(,)? }) => {
        pub mod $module {
            pub const NS: &str = $base;
            $(pub const $name: &str = concat!($base, $local);)*
            pub const ALL: &[&str] = &[$($name),*];
        }
    };
}

ns!(ops, "https://ograph.dev/ops#", {
    APPLICATION = "Application",
    TEAM = "Team",
    REGION = "Region",
    BUSINESS_PROCESS = "BusinessProcess",
    DEPENDS_ON = "dependsOn",
    OWNED_BY = "ownedBy",
    IN_REGION = "inRegion",
    SUPPORTS = "supports",
    NAME = "name",
    TIER = "tier",
    ALIAS = "alias",
    VENDOR = "Vendor",
    USES_VENDOR = "usesVendor",
});

ns!(evt, "https://ograph.dev/evt#", {
    EVENT = "Event",
    AT = "at",
    ON = "on",
    SIGNAL = "signal",
    SEVERITY = "severity",
    SUMMARY = "summary",
    SOURCE_RECORD = "sourceRecord",
    DEPLOYMENT = "Deployment",
    CONFIG_CHANGE = "ConfigChange",
    ALERT = "Alert",
    LOG_ERROR = "LogError",
    METRIC_ANOMALY = "MetricAnomaly",
    TRACE_FAILED = "TraceFailed",
    TRACE_SLOW = "TraceSlow",
    VERSION = "version",
    TEMPLATE = "template",
    METRIC = "metric",
    VALUE = "value",
    COUNT = "count",
    UNTIL = "until",
    NEWS_ITEM = "NewsItem",
    MENTIONS = "mentions",
    E2E_FAILURE = "E2EFailure",
    EXPERIMENT = "Experiment",
    COMMIT = "Commit",
    EXCEPTION = "Exception",
    REVISION = "revision",
    CHANGED_FILE = "changedFile",
    CODE_LOCATION = "codeLocation",
});

ns!(inc, "https://ograph.dev/inc#", {
    INCIDENT = "Incident",
    ID = "id",
    TITLE = "title",
    STATUS = "status",
    RESOLVED_AT = "resolvedAt",
    MITIGATION = "mitigation",
    CAUSE = "cause",
    FAULT_TYPE = "faultType",
    FAULT_CLASS = "faultClass",
});

ns!(disc, "https://ograph.dev/disc#", {
    THREAD = "Thread",
    ABOUT = "about",
    CATEGORY = "category",
    SENTIMENT = "sentiment",
    TEXT_HASH = "textHash",
    CHANNEL = "channel",
    AT = "at",
});

ns!(corr, "https://ograph.dev/corr#", {
    CHAIN = "Chain",
    ROOT_CAUSE = "rootCause",
    LEADS_TO = "leadsTo",
    LINK = "link",
    CONFIDENCE = "confidence",
    RANK = "rank",
    CANDIDATE_APP = "candidateApp",
    SETTINGS = "settings",
});

ns!(rca, "https://ograph.dev/rca#", {
    VERDICT = "Verdict",
    ON_CHAIN = "onChain",
    STATE = "state",
    BY = "by",
    EVIDENCE = "evidence",
    AT = "at",
    ROOT_CLASS = "rootClass",
    ROOT_ON = "rootOn",
    ROOT_AT = "rootAt",
    ROOT_SIGNAL = "rootSignal",
    SUPERSEDED_BY = "supersededBy",
    RANK = "rank",
    SETTINGS = "settings",
});

ns!(harv, "https://ograph.dev/harv#", {
    HARVEST = "Harvest",
    SOURCE = "source",
    AT = "at",
    EXTRACTOR = "extractor",
    METHOD = "method",
    MODEL = "model",
    PROMPT_HASH = "promptHash",
    QUARANTINED = "Quarantined",
    LLM_FACT = "LlmFact",
    REASON = "reason",
    FACT = "fact",
    STATUS = "status",
    FILE = "file",
});

pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
pub const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
pub const DATA: &str = "https://ograph.dev/id/";
pub const GRAPH: &str = "https://ograph.dev/graph/";
pub const QUARANTINE: &str = "https://ograph.dev/graph/quarantine";

pub fn all() -> impl Iterator<Item = &'static str> {
    [ops::ALL, evt::ALL, inc::ALL, disc::ALL, corr::ALL, rca::ALL, harv::ALL].into_iter().flatten().copied()
}

pub const PREFIXES: &[(&str, &str)] = &[
    ("ops", ops::NS),
    ("evt", evt::NS),
    ("inc", inc::NS),
    ("disc", disc::NS),
    ("corr", corr::NS),
    ("rca", rca::NS),
    ("harv", harv::NS),
    ("id", DATA),
    ("g", GRAPH),
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
    ("owl", "http://www.w3.org/2002/07/owl#"),
    ("sh", "http://www.w3.org/ns/shacl#"),
];

pub fn compact(iri: &str) -> String {
    for (p, ns) in PREFIXES {
        if let Some(local) = iri.strip_prefix(ns) {
            return format!("{p}:{local}");
        }
    }
    format!("<{iri}>")
}

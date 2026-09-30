//! Tracks which contract operations the server implements, so a contract change
//! cannot silently add work nobody triaged.
mod common;
use common::Contract;

/// Operation ids with a handler and a conformance test. Move ids here as milestones land.
const IMPLEMENTED: &[&str] = &[
    "getHealth",
    "getServer",
    "updateServer",
    "streamEvents",
    "listDevices",
    "updateDevice",
    "listAudit",
    "listListeners",
    "createListener",
    "getListener",
    "renameListener",
    "deleteListener",
    "getListenerImpact",
    "getListenerSettings",
    "putListenerSettings",
    "listBooks",
    "findDuplicateBooks",
    "createImport",
    "getImport",
    "cancelImport",
    "createSampleBook",
    "getBook",
    "updateBook",
    "getBookCover",
    "refreshBookCover",
    "removeBook",
    "restoreBook",
    "listSeries",
    "listChapters",
    "getChapterText",
    "searchBook",
    "getPlace",
    "putPlace",
    "clearPlace",
    "setFinished",
    "listPlaceHistory",
    "listVoiceSources",
    "getVoiceSource",
    "configureVoiceSource",
    "testVoiceSource",
    "refreshVoiceSource",
    "removeVoiceSource",
    "listVoices",
    "listAudiobooks",
    "createAudiobook",
    "getAudiobook",
    "listAudiobookChapters",
    "requestChapterAudio",
    "makeAudiobookReady",
    "getAudio",
    "getAudioTimings",
    "getVoiceSample",
    "listJobs",
    "getJob",
    "pauseJob",
    "resumeJob",
    "cancelJob",
    "getAllowance",
    "putAllowance",
    "listPrices",
    "refreshPrices",
    "putPriceTable",
];

#[test]
fn implemented_operations_exist_in_the_contract() {
    let c = Contract::load();
    let ids = c.operation_ids();
    for id in IMPLEMENTED {
        assert!(ids.contains(&id.to_string()), "{id} is not in the contract");
    }
    eprintln!(
        "implemented {} of {} operations",
        IMPLEMENTED.len(),
        ids.len()
    );
}

use block::ConcreteBlock;
use cocoa::base::{id, nil, YES};
use cocoa::foundation::{NSAutoreleasePool, NSString};
use hound::{SampleFormat, WavSpec, WavWriter};
use objc::{class, msg_send, sel, sel_impl};
use std::ffi::CStr;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const AUTH_STATUS_NOT_DETERMINED: i64 = 0;
const AUTH_STATUS_DENIED: i64 = 1;
const AUTH_STATUS_RESTRICTED: i64 = 2;
const AUTH_STATUS_AUTHORIZED: i64 = 3;
const TASK_HINT_DICTATION: i64 = 1;
const MAX_CONTEXTUAL_STRINGS: usize = 100;

#[link(name = "Speech", kind = "framework")]
extern "C" {}

struct OwnedId(id);

impl OwnedId {
    unsafe fn new(value: id, context: &str) -> Result<Self, String> {
        if value == nil {
            Err(format!("{context} unavailable"))
        } else {
            Ok(Self(value))
        }
    }

    fn as_id(&self) -> id {
        self.0
    }
}

impl Drop for OwnedId {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![self.0, release];
        }
    }
}

#[derive(Default)]
struct RecognitionState {
    outcome: Option<Result<String, String>>,
    best_text: String,
}

pub(crate) async fn transcribe_audio(
    samples: Vec<f32>,
    sample_rate: u32,
    language: Option<String>,
    contextual_strings: Vec<String>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        transcribe_audio_blocking(samples, sample_rate, language, contextual_strings)
    })
    .await
    .map_err(|err| format!("Apple Speech task failed: {err}"))?
}

fn transcribe_audio_blocking(
    samples: Vec<f32>,
    sample_rate: u32,
    language: Option<String>,
    contextual_strings: Vec<String>,
) -> Result<String, String> {
    if sample_rate == 0 {
        return Err("Apple Speech transcription requires a positive sample rate.".to_string());
    }

    if samples.is_empty() {
        return Ok(String::new());
    }

    let path = write_temp_wav(&samples, sample_rate)?;
    let timeout = recognition_timeout(samples.len(), sample_rate);
    let result = unsafe { recognize_wav(&path, language.as_deref(), contextual_strings, timeout) };
    let _ = std::fs::remove_file(&path);
    result.map(|text| text.trim().to_string())
}

fn write_temp_wav(samples: &[f32], sample_rate: u32) -> Result<PathBuf, String> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| format!("Failed to create Apple Speech temp file name: {err}"))?
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "voquill-apple-speech-{unique}-{}.wav",
        std::process::id()
    ));

    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::create(&path, spec)
        .map_err(|err| format!("Failed to create Apple Speech WAV file: {err}"))?;

    for sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let value = (clamped * i16::MAX as f32).round() as i16;
        writer
            .write_sample(value)
            .map_err(|err| format!("Failed to write Apple Speech WAV sample: {err}"))?;
    }

    writer
        .finalize()
        .map_err(|err| format!("Failed to finalize Apple Speech WAV file: {err}"))?;

    Ok(path)
}

fn recognition_timeout(sample_count: usize, sample_rate: u32) -> Duration {
    let audio_seconds = sample_count as f64 / sample_rate as f64;
    let timeout_seconds = (audio_seconds * 3.0 + 30.0).clamp(30.0, 240.0);
    Duration::from_secs_f64(timeout_seconds)
}

unsafe fn recognize_wav(
    path: &Path,
    language: Option<&str>,
    contextual_strings: Vec<String>,
    timeout: Duration,
) -> Result<String, String> {
    let pool = NSAutoreleasePool::new(nil);
    let result = (|| {
        ensure_speech_authorized()?;

        let recognizer = create_recognizer(language)?;
        let supports_on_device: bool = msg_send![recognizer.as_id(), supportsOnDeviceRecognition];
        if !supports_on_device {
            return Err(
                "Apple Speech on-device recognition is not available for this language."
                    .to_string(),
            );
        }

        let available: bool = msg_send![recognizer.as_id(), isAvailable];
        if !available {
            return Err("Apple Speech recognizer is not available right now.".to_string());
        }

        let path_string = OwnedId::new(
            NSString::alloc(nil).init_str(&path.to_string_lossy()),
            "Apple Speech file path",
        )?;
        let url: id = msg_send![class!(NSURL), fileURLWithPath: path_string.as_id()];
        if url == nil {
            return Err("Failed to create Apple Speech file URL.".to_string());
        }

        let request_alloc: id = msg_send![class!(SFSpeechURLRecognitionRequest), alloc];
        let request = OwnedId::new(
            msg_send![request_alloc, initWithURL: url],
            "Apple Speech recognition request",
        )?;

        let _: () = msg_send![request.as_id(), setShouldReportPartialResults: YES];
        let _: () = msg_send![request.as_id(), setRequiresOnDeviceRecognition: YES];
        let _: () = msg_send![request.as_id(), setTaskHint: TASK_HINT_DICTATION];

        set_adds_punctuation_if_available(request.as_id());
        let contextual_array = create_contextual_strings(&contextual_strings)?;
        if let Some(array) = contextual_array.as_ref() {
            let _: () = msg_send![request.as_id(), setContextualStrings: array.as_id()];
        }

        let state = Arc::new((Mutex::new(RecognitionState::default()), Condvar::new()));
        let callback_state = Arc::clone(&state);
        let handler = ConcreteBlock::new(move |result: id, error: id| {
            let mut next_text: Option<String> = None;
            let mut is_final = false;
            let mut next_error: Option<String> = None;

            unsafe {
                if result != nil {
                    let transcription: id = msg_send![result, bestTranscription];
                    if transcription != nil {
                        let formatted: id = msg_send![transcription, formattedString];
                        next_text = nsstring_to_string(formatted);
                    }
                    is_final = msg_send![result, isFinal];
                }

                if error != nil {
                    next_error = Some(error_to_string(error));
                }
            }

            let (lock, cvar) = &*callback_state;
            if let Ok(mut state) = lock.lock() {
                if let Some(text) = next_text {
                    if is_final {
                        state.best_text = select_final_transcript(&text, &state.best_text);
                    } else if is_better_partial_transcript(&text, &state.best_text) {
                        state.best_text = text;
                    }
                }

                if state.outcome.is_none() {
                    if let Some(message) = next_error {
                        state.outcome = Some(if state.best_text.trim().is_empty() {
                            Err(message)
                        } else {
                            Ok(state.best_text.clone())
                        });
                        cvar.notify_one();
                    } else if is_final {
                        state.outcome = Some(Ok(state.best_text.clone()));
                        cvar.notify_one();
                    }
                }
            }
        })
        .copy();

        let task: id = msg_send![
            recognizer.as_id(),
            recognitionTaskWithRequest: request.as_id()
            resultHandler: &*handler
        ];
        if task == nil {
            return Err("Failed to start Apple Speech recognition task.".to_string());
        }
        let task = OwnedId::new(msg_send![task, retain], "Apple Speech recognition task")?;

        let outcome = wait_for_recognition(&state, timeout);
        if outcome.is_err() {
            let _: () = msg_send![task.as_id(), cancel];
        }

        outcome?
    })();
    pool.drain();
    result
}

unsafe fn ensure_speech_authorized() -> Result<(), String> {
    let status: i64 = msg_send![class!(SFSpeechRecognizer), authorizationStatus];
    if status == AUTH_STATUS_AUTHORIZED {
        return Ok(());
    }

    if status == AUTH_STATUS_NOT_DETERMINED && !is_running_from_app_bundle() {
        return Err(
            "Apple Speech permission is not granted. macOS only shows the Speech Recognition permission prompt for app bundles, not the bare dev executable."
                .to_string(),
        );
    }

    let pair = Arc::new((Mutex::new(None::<i64>), Condvar::new()));
    let pair_clone = Arc::clone(&pair);
    let handler = ConcreteBlock::new(move |auth_status: i64| {
        let (lock, cvar) = &*pair_clone;
        if let Ok(mut slot) = lock.lock() {
            *slot = Some(auth_status);
            cvar.notify_one();
        }
    })
    .copy();

    let _: () = msg_send![class!(SFSpeechRecognizer), requestAuthorization: &*handler];

    let (lock, cvar) = &*pair;
    let mut result = lock
        .lock()
        .map_err(|_| "Apple Speech authorization mutex poisoned.".to_string())?;
    while result.is_none() {
        result = cvar
            .wait(result)
            .map_err(|_| "Apple Speech authorization mutex poisoned.".to_string())?;
    }

    match result.unwrap_or(status) {
        AUTH_STATUS_AUTHORIZED => Ok(()),
        AUTH_STATUS_DENIED => Err("Apple Speech recognition permission was denied.".to_string()),
        AUTH_STATUS_RESTRICTED => {
            Err("Apple Speech recognition is restricted on this Mac.".to_string())
        }
        _ => Err("Apple Speech recognition permission was not granted.".to_string()),
    }
}

unsafe fn is_running_from_app_bundle() -> bool {
    let bundle: id = msg_send![class!(NSBundle), mainBundle];
    if bundle == nil {
        return false;
    }

    let bundle_path: id = msg_send![bundle, bundlePath];
    if nsstring_to_string(bundle_path)
        .map(|path| path.ends_with(".app") || path.contains(".app/"))
        .unwrap_or(false)
    {
        return true;
    }

    let executable_path: id = msg_send![bundle, executablePath];
    nsstring_to_string(executable_path)
        .map(|path| path.contains(".app/Contents/MacOS/"))
        .unwrap_or(false)
}

unsafe fn create_recognizer(language: Option<&str>) -> Result<OwnedId, String> {
    let recognizer_alloc: id = msg_send![class!(SFSpeechRecognizer), alloc];

    if let Some(locale_identifier) = normalize_locale_identifier(language) {
        let locale_identifier = OwnedId::new(
            NSString::alloc(nil).init_str(&locale_identifier),
            "Apple Speech locale identifier",
        )?;
        let locale_alloc: id = msg_send![class!(NSLocale), alloc];
        let locale = OwnedId::new(
            msg_send![
                locale_alloc,
                initWithLocaleIdentifier: locale_identifier.as_id()
            ],
            "Apple Speech locale",
        )?;

        return OwnedId::new(
            msg_send![recognizer_alloc, initWithLocale: locale.as_id()],
            "Apple Speech recognizer",
        );
    }

    OwnedId::new(
        msg_send![recognizer_alloc, initWithLocale: nil],
        "Apple Speech recognizer",
    )
}

fn normalize_locale_identifier(language: Option<&str>) -> Option<String> {
    let value = language?.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("auto") {
        return None;
    }
    Some(value.replace('_', "-"))
}

unsafe fn set_adds_punctuation_if_available(request: id) {
    let responds: bool = msg_send![request, respondsToSelector: sel!(setAddsPunctuation:)];
    if responds {
        let _: () = msg_send![request, setAddsPunctuation: YES];
    }
}

unsafe fn create_contextual_strings(values: &[String]) -> Result<Option<OwnedId>, String> {
    let sanitized: Vec<String> = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .take(MAX_CONTEXTUAL_STRINGS)
        .map(ToOwned::to_owned)
        .collect();

    if sanitized.is_empty() {
        return Ok(None);
    }

    let array: id = msg_send![class!(NSMutableArray), arrayWithCapacity: sanitized.len()];
    if array == nil {
        return Err("Failed to create Apple Speech contextual string array.".to_string());
    }

    for value in sanitized {
        let item = OwnedId::new(
            NSString::alloc(nil).init_str(&value),
            "Apple Speech contextual string",
        )?;
        let _: () = msg_send![array, addObject: item.as_id()];
    }

    Ok(Some(OwnedId::new(
        msg_send![array, retain],
        "Apple Speech contextual strings",
    )?))
}

fn wait_for_recognition(
    state: &Arc<(Mutex<RecognitionState>, Condvar)>,
    timeout: Duration,
) -> Result<Result<String, String>, String> {
    let (lock, cvar) = &**state;
    let mut guard = lock
        .lock()
        .map_err(|_| "Apple Speech recognition mutex poisoned.".to_string())?;
    while guard.outcome.is_none() {
        let (next_guard, timeout_result) = cvar
            .wait_timeout(guard, timeout)
            .map_err(|_| "Apple Speech recognition mutex poisoned.".to_string())?;
        guard = next_guard;
        if timeout_result.timed_out() && guard.outcome.is_none() {
            return Err("Apple Speech recognition timed out.".to_string());
        }
    }

    Ok(guard
        .outcome
        .take()
        .unwrap_or_else(|| Err("Apple Speech recognition ended without a result.".to_string())))
}

fn select_final_transcript(final_text: &str, best_partial: &str) -> String {
    let final_text = final_text.trim();
    let best_partial = best_partial.trim();

    if final_text.is_empty() || is_suspiciously_short_final(final_text, best_partial) {
        return best_partial.to_string();
    }

    final_text.to_string()
}

fn is_suspiciously_short_final(final_text: &str, best_partial: &str) -> bool {
    let final_words = final_text.split_whitespace().count();
    let partial_words = best_partial.split_whitespace().count();

    partial_words >= 6
        && final_words + 2 < partial_words
        && final_text.chars().count() * 2 < best_partial.chars().count()
}

fn is_better_partial_transcript(candidate: &str, current: &str) -> bool {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        return false;
    }

    let current = current.trim();
    if current.is_empty() {
        return true;
    }

    candidate.split_whitespace().count() > current.split_whitespace().count()
        || candidate.chars().count() > current.chars().count()
}

unsafe fn nsstring_to_string(value: id) -> Option<String> {
    if value == nil {
        return None;
    }

    let utf8: *const c_char = msg_send![value, UTF8String];
    if utf8.is_null() {
        return None;
    }

    Some(CStr::from_ptr(utf8).to_string_lossy().into_owned())
}

unsafe fn error_to_string(error: id) -> String {
    let description: id = msg_send![error, localizedDescription];
    nsstring_to_string(description)
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| "Apple Speech recognition failed.".to_string())
}

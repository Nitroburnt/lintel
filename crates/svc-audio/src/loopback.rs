// Loopback (docs/ARCHITECTURE.md §8): what an output plays, as it plays, for
// whoever has to hear it: the equalizer's bars, the beat, a recording's sound.
// Nothing here runs by itself and nothing of the sound is kept: `loopback`
// opens a WASAPI loopback capture and `read` hands over what has arrived, on
// the caller's thread and on the caller's time. Dropping the `Loopback` closes
// it. Windows asks no permission for it and shows no indicator.
//
// The frames are 32-bit float, interleaved, 48 kHz stereo: Windows converts
// from whatever the output runs at. Should it refuse, the frames come in the
// output's own format instead: read `rate` and `channels`, never assume.
//
// While nothing plays on the output Windows delivers nothing at all, not
// silence: `read` then comes back with 0 after its wait.

use std::marker::PhantomData;
use std::sync::mpsc;
use std::time::Duration;

use windows::core::{implement, Interface, Ref, GUID, HRESULT};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE, WAIT_FAILED};
use windows::Win32::Media::Audio::{
    eConsole, eRender, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient,
    IAudioClient, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoTaskMemFree, BLOB, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

use crate::{enumerator, text, Com, WinResult};

/// What is asked of Windows: the rate every encoder and every meter is happy with.
const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// Room for what plays between two reads, in 100 ns units (0.2 s).
const BUFFER: i64 = 2_000_000;

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
const BUFFERFLAGS_SILENT: u32 = 2;

/// An open loopback capture. It stays on the thread that opened it.
pub struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    /// Set by Windows whenever sound has arrived. Closed after the client
    /// that was given it is released: the fields go in this order.
    event: Event,
    rate: u32,
    channels: u16,
    output: (u32, u16),
    /// The capture reads on the thread that opened it, where COM was entered:
    /// said here rather than left to whatever the fields happen to be.
    _thread: PhantomData<*mut ()>,
    // Last: COM is left after the interfaces above are released.
    _com: Com,
}

/// Opens what the default output plays (`None`), or only what one process and
/// its child processes play (`Some(pid)`: Windows 10 2004 and later; an error
/// before that). Capturing starts at once; nothing is heard of it anywhere.
pub fn loopback(pid: Option<u32>) -> Result<Loopback, String> {
    let com = Com::enter();
    open(pid, com).map_err(text)
}

fn open(pid: Option<u32>, com: Com) -> WinResult<Loopback> {
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    let convert = flags | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    let asked = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: CHANNELS,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * u32::from(CHANNELS) * 4,
        nBlockAlign: CHANNELS * 4,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    unsafe {
        // Made before the client, so that it is released after it: a client that
        // fails to start must not have its event handle closed underneath it.
        let event = Event(CreateEventW(None, false, false, None)?);
        let (client, rate, channels, output) = match pid {
            // A process has no format of its own: it is given ours or nothing.
            Some(pid) => {
                let client = process_client(pid)?;
                client.Initialize(AUDCLNT_SHAREMODE_SHARED, convert, BUFFER, 0, &asked, None)?;
                (client, RATE, CHANNELS, (0, 0))
            }
            None => {
                let device = enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole)?;
                let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
                let mix = Mix(client.GetMixFormat()?);
                let output = (mix.rate(), mix.channels());
                if client.Initialize(AUDCLNT_SHAREMODE_SHARED, convert, BUFFER, 0, &asked, None).is_ok() {
                    (client, RATE, CHANNELS, output)
                } else {
                    // No conversion on this output: its own format, when that is float.
                    // A client that failed to initialise is not tried again.
                    let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
                    if !mix.is_float() {
                        return Err(windows::core::Error::new(E_FAIL, "the output's format is not 32-bit float"));
                    }
                    client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER, 0, mix.0, None)?;
                    (client, output.0, output.1, output)
                }
            }
        };
        client.SetEventHandle(event.0)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok(Loopback { client, capture, event, rate, channels, output, _thread: PhantomData, _com: com })
    }
}

struct Event(HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl Loopback {
    /// Frames a second.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Samples in a frame (`read` interleaves them).
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// The rate and channels the output itself runs at, before any conversion;
    /// (0, 0) for a process.
    pub fn output_format(&self) -> (u32, u16) {
        self.output
    }

    /// Appends what has been played since the last call to `out`, as samples
    /// from -1 to 1, and says how many frames that was. With nothing waiting it
    /// sleeps until sound arrives, `wait` at most: 0 then means nothing plays.
    /// An error means the output went away (unplugged, its format changed):
    /// drop this and open another.
    pub fn read(&mut self, out: &mut Vec<f32>, wait: Duration) -> Result<usize, String> {
        let mut frames = self.drain(out).map_err(text)?;
        if frames == 0 {
            let waited = unsafe { WaitForSingleObject(self.event.0, millis(wait)) };
            if waited == WAIT_FAILED {
                return Err(text(windows::core::Error::from_win32()));
            }
            frames = self.drain(out).map_err(text)?;
        }
        Ok(frames)
    }

    fn drain(&mut self, out: &mut Vec<f32>) -> WinResult<usize> {
        let mut total = 0;
        unsafe {
            while self.capture.GetNextPacketSize()? > 0 {
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut::<u8>(), 0u32, 0u32);
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let (at, samples) = (out.len(), frames as usize * usize::from(self.channels));
                out.resize(at + samples, 0.0);
                // Safe by what `open` asked for and checked: 48 kHz stereo 32-bit float
                // with conversion, or the output's own format when `is_float` passed, so
                // a frame of the packet is exactly `channels` × 4 bytes.
                if flags & BUFFERFLAGS_SILENT == 0 && !data.is_null() {
                    std::ptr::copy_nonoverlapping(data, out[at..].as_mut_ptr().cast::<u8>(), samples * 4);
                }
                self.capture.ReleaseBuffer(frames)?;
                total += frames as usize;
            }
        }
        Ok(total)
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// The output's own format, freed when done with.
struct Mix(*mut WAVEFORMATEX);

impl Mix {
    fn rate(&self) -> u32 {
        unsafe { (*self.0).nSamplesPerSec }
    }
    fn channels(&self) -> u16 {
        unsafe { (*self.0).nChannels }
    }
    fn is_float(&self) -> bool {
        unsafe {
            let format = *self.0;
            let (tag, bits) = (format.wFormatTag, format.wBitsPerSample);
            let float = tag == WAVE_FORMAT_IEEE_FLOAT
                || (tag == WAVE_FORMAT_EXTENSIBLE
                    // The subtype is past the base struct: only there when it is said to be.
                    && format.cbSize >= 22
                    && {
                        // The extensible struct is packed: its field is read, never referenced.
                        let ext = self.0.cast::<WAVEFORMATEXTENSIBLE>();
                        std::ptr::addr_of!((*ext).SubFormat).read_unaligned() == SUBTYPE_IEEE_FLOAT
                    });
            float && bits == 32
        }
    }
}

impl Drop for Mix {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 as *const _)) };
    }
}

/// The audio client of one process tree. Windows activates it on a thread of
/// its own and says when it is there.
unsafe fn process_client(pid: u32) -> WinResult<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // A blob that points at `params`. It is borrowed: the variant must never be
    // dropped, or it would free what is on this stack.
    let mut how = std::mem::ManuallyDrop::new(PROPVARIANT::default());
    unsafe {
        let inner = &mut *how.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: (&raw const params).cast_mut().cast(),
        };
    }
    let (tx, rx) = mpsc::channel();
    let done: IActivateAudioInterfaceCompletionHandler = Activated(tx).into();
    let pending =
        unsafe { ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*how), &done)? };
    // Waited for without a limit: `how` points at `params` on this stack, and Windows
    // may read them until the activation completes. A timeout here would return with
    // that pointer dangling and leave the completion to dereference a dead frame.
    if rx.recv().is_err() {
        return Err(windows::core::Error::new(E_FAIL, "the process's sound did not open"));
    }
    let (mut result, mut client) = (HRESULT(0), None);
    unsafe { pending.GetActivateResult(&mut result, &mut client)? };
    result.ok()?;
    client.ok_or_else(|| windows::core::Error::from(E_FAIL))?.cast()
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(mpsc::Sender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _pending: Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> WinResult<()> {
        let _ = self.0.send(());
        Ok(())
    }
}

/// `wait` in whole milliseconds, rounded up so that a sub-millisecond wait is a wait
/// and not a poll, and capped so that a caller cannot park this thread for minutes.
fn millis(wait: Duration) -> u32 {
    wait.as_micros().div_ceil(1000).min(60_000) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_wait_is_not_a_poll() {
        assert_eq!(millis(Duration::ZERO), 0, "no wait is a poll, and that is asked for");
        assert_eq!(millis(Duration::from_micros(1)), 1);
        assert_eq!(millis(Duration::from_micros(500)), 1);
        assert_eq!(millis(Duration::from_millis(100)), 100);
        assert_eq!(millis(Duration::from_micros(100_500)), 101, "rounded up, never down");
        assert_eq!(millis(Duration::from_secs(30)), 30_000, "a real wait is kept");
        assert_eq!(millis(Duration::from_secs(120)), 60_000, "capped");
    }
}

use std::{
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
  time::Duration,
};

use dashmap::DashMap;
use napi::{
  bindgen_prelude::ObjectFinalize,
  threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode},
  Env, JsFunction, NapiRaw,
};
use windows::{
  core::HSTRING,
  Foundation::{EventRegistrationToken, TimeSpan, TypedEventHandler, Uri},
  Media::{
    MediaPlaybackStatus, MediaPlaybackType, MusicDisplayProperties,
    Playback::MediaPlayer as WindowsMediaPlayer, PlaybackPositionChangeRequestedEventArgs,
    SystemMediaTransportControls, SystemMediaTransportControlsButton,
    SystemMediaTransportControlsButtonPressedEventArgs,
    SystemMediaTransportControlsDisplayUpdater, SystemMediaTransportControlsTimelineProperties,
  },
  Storage::{StorageFile, Streams::RandomAccessStreamReference},
};

/// Default relative seek amount for SMTC FastForward / Rewind buttons.
const DEFAULT_SEEK_STEP_SECS: f64 = 15.0;

type ButtonListeners = Arc<DashMap<usize, ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>>;
type PositionListeners = Arc<DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>>;

#[napi]
#[derive(Debug, PartialEq, Eq)]
pub enum MediaPlayerThumbnailType {
  Unknown = -1,
  File = 1,
  Uri = 2,
}

#[napi]
#[derive(Debug, PartialEq, Eq)]
pub enum MediaPlayerMediaType {
  Unknown = -1,
  Music = 1,
}

#[napi]
#[derive(Debug, PartialEq, Eq)]
pub enum MediaPlayerPlaybackStatus {
  Unknown = -1,
  Playing = 1,
  Paused = 2,
  Stopped = 3,
}

#[napi]
struct MediaPlayerThumbnail {
  thumbnail_type: MediaPlayerThumbnailType,
  stream_ref: RandomAccessStreamReference,
}

#[napi]
impl MediaPlayerThumbnail {
  #[napi(factory)]
  #[allow(dead_code)]
  pub async fn create(
    thumbnail_type: MediaPlayerThumbnailType,
    thumbnail: String,
  ) -> napi::Result<Self> {
    let stream_ref = match thumbnail_type {
      MediaPlayerThumbnailType::File => {
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(thumbnail))
          .map_err(map_windows_error)?
          .await
          .map_err(map_windows_error)?;
        RandomAccessStreamReference::CreateFromFile(&file).map_err(map_windows_error)?
      }
      MediaPlayerThumbnailType::Uri => {
        let uri = Uri::CreateUri(&HSTRING::from(thumbnail)).map_err(map_windows_error)?;
        RandomAccessStreamReference::CreateFromUri(&uri).map_err(map_windows_error)?
      }
      _ => {
        return Err(napi::Error::from_reason(format!(
          "{:?} is not a valid MediaPlayerThumbnailType to create",
          thumbnail_type
        )))
      }
    };

    Ok(Self {
      thumbnail_type,
      stream_ref,
    })
  }

  #[napi(getter, js_name = "type")]
  #[allow(dead_code)]
  pub fn thumbnail_type(&self) -> MediaPlayerThumbnailType {
    self.thumbnail_type
  }
}

#[napi(custom_finalize)]
struct MediaPlayer {
  player: WindowsMediaPlayer,
  smtc_button_pressed_registration: EventRegistrationToken,
  smtc_playback_position_changed_registration: EventRegistrationToken,
  button_pressed_listeners: ButtonListeners,
  playback_position_changed_listeners: PositionListeners,
  playback_position_seeked_listeners: PositionListeners,
  seek_enabled: Arc<AtomicBool>,
  track_id: String,
}

#[napi]
impl MediaPlayer {
  #[napi(constructor)]
  #[allow(dead_code)]
  pub fn new(service_name: String, _identity: String) -> napi::Result<Self> {
    let button_pressed_listeners: ButtonListeners = Arc::new(DashMap::new());
    let playback_position_changed_listeners: PositionListeners = Arc::new(DashMap::new());
    let playback_position_seeked_listeners: PositionListeners = Arc::new(DashMap::new());

    let player = WindowsMediaPlayer::new().map_err(map_windows_error)?;
    let smtc = player
      .SystemMediaTransportControls()
      .map_err(map_windows_error)?;

    let button_listeners = button_pressed_listeners.clone();
    let seeked_listeners = playback_position_seeked_listeners.clone();
    let seek_enabled = Arc::new(AtomicBool::new(true));
    let seek_gate_for_buttons = seek_enabled.clone();
    let button_handler = TypedEventHandler::<
      SystemMediaTransportControls,
      SystemMediaTransportControlsButtonPressedEventArgs,
    >::new(move |_sender, args| {
      if let Some(args) = args {
        if let Ok(button) = args.Button() {
          dispatch_smtc_button(
            button,
            &button_listeners,
            &seeked_listeners,
            seek_gate_for_buttons.load(Ordering::Relaxed),
          );
        }
      }
      Ok(())
    });

    let button_pressed_registration = smtc
      .ButtonPressed(&button_handler)
      .map_err(map_windows_error)?;

    let position_listeners = playback_position_changed_listeners.clone();
    let seek_gate_for_handler = seek_enabled.clone();
    let position_handler = TypedEventHandler::<
      SystemMediaTransportControls,
      PlaybackPositionChangeRequestedEventArgs,
    >::new(move |_sender, args| {
      if !seek_gate_for_handler.load(Ordering::Relaxed) {
        return Ok(());
      }
      if let Some(args) = args {
        if let Ok(requested_playback_position) = args.RequestedPlaybackPosition() {
          emit_f64(
            &position_listeners,
            Duration::from(requested_playback_position).as_secs_f64(),
          );
        }
      }
      Ok(())
    });

    let playback_position_changed_registration = smtc
      .PlaybackPositionChangeRequested(&position_handler)
      .map_err(map_windows_error)?;

    smtc
      .DisplayUpdater()
      .map_err(map_windows_error)?
      .SetAppMediaId(&HSTRING::from(service_name))
      .map_err(map_windows_error)?;

    Ok(Self {
      player,
      button_pressed_listeners,
      playback_position_changed_listeners,
      playback_position_seeked_listeners,
      smtc_button_pressed_registration: button_pressed_registration,
      smtc_playback_position_changed_registration: playback_position_changed_registration,
      seek_enabled,
      track_id: String::new(),
    })
  }

  /// Activates the MediaPlayer allowing the operating system to see and use it
  #[napi]
  #[allow(dead_code)]
  pub fn activate(&self) -> napi::Result<()> {
    self
      .smtc()?
      .SetIsEnabled(true)
      .map_err(map_windows_error)
  }

  /// Deactivates the MediaPlayer denying the operating system to see and use it
  #[napi]
  #[allow(dead_code)]
  pub fn deactivate(&self) -> napi::Result<()> {
    let smtc = self.smtc()?;
    // Closed is the SMTC-recommended status when the session is no longer active.
    let _ = smtc.SetPlaybackStatus(MediaPlaybackStatus::Closed);
    smtc.SetIsEnabled(false).map_err(map_windows_error)
  }

  /// Adds an event listener to the MediaPlayer
  ///
  /// 'buttonpressed' - Emitted when a media services button is pressed
  /// 'positionchanged' - Emitted when the media service requests a position change
  /// 'positionseeked' - Emitted when the media service requests a forward or backward position seek from current position
  #[napi]
  #[allow(dead_code)]
  pub fn add_event_listener(
    &mut self,
    env: Env,
    #[napi(ts_arg_type = "'buttonpressed' | 'positionchanged' | 'positionseeked'")]
    event_name: String,
    callback: JsFunction,
  ) -> napi::Result<()> {
    let callback_ptr = unsafe { callback.raw() as usize };

    match event_name.as_str() {
      "buttonpressed" => {
        insert_string_listener(&self.button_pressed_listeners, env, callback_ptr, callback)?
      }
      "positionchanged" => insert_f64_listener(
        &self.playback_position_changed_listeners,
        env,
        callback_ptr,
        callback,
      )?,
      "positionseeked" => insert_f64_listener(
        &self.playback_position_seeked_listeners,
        env,
        callback_ptr,
        callback,
      )?,
      _ => {}
    }

    Ok(())
  }

  /// Removes an event listener from the MediaPlayer
  #[napi]
  #[allow(dead_code)]
  pub fn remove_event_listener(
    &mut self,
    #[napi(ts_arg_type = "'buttonpressed' | 'positionchanged' | 'positionseeked'")]
    event_name: String,
    callback: JsFunction,
  ) -> napi::Result<()> {
    let callback_ptr = unsafe { callback.raw() as usize };

    match event_name.as_str() {
      "buttonpressed" => {
        self.button_pressed_listeners.remove(&callback_ptr);
      }
      "positionchanged" => {
        self
          .playback_position_changed_listeners
          .remove(&callback_ptr);
      }
      "positionseeked" => {
        self.playback_position_seeked_listeners.remove(&callback_ptr);
      }
      _ => {}
    }

    Ok(())
  }

  /// Adds an event listener to the MediaPlayer
  ///
  /// Alias for addEventListener
  #[napi]
  #[allow(dead_code)]
  pub fn on(
    &mut self,
    env: Env,
    #[napi(ts_arg_type = "'buttonpressed' | 'positionchanged' | 'positionseeked'")]
    event_name: String,
    callback: JsFunction,
  ) -> napi::Result<()> {
    self.add_event_listener(env, event_name, callback)
  }

  /// Removes an event listener from the MediaPlayer
  ///
  /// Alias for removeEventListener
  #[napi]
  #[allow(dead_code)]
  pub fn off(
    &mut self,
    #[napi(ts_arg_type = "'buttonpressed' | 'positionchanged' | 'positionseeked'")]
    event_name: String,
    callback: JsFunction,
  ) -> napi::Result<()> {
    self.remove_event_listener(event_name, callback)
  }

  /// Instructs the media service to update its media information being displayed
  #[napi]
  #[allow(dead_code)]
  pub fn update(&self) -> napi::Result<()> {
    self
      .display_updater()?
      .Update()
      .map_err(map_windows_error)
  }

  /// Sets the thumbnail
  #[napi]
  #[allow(dead_code)]
  pub fn set_thumbnail(&mut self, thumbnail: &MediaPlayerThumbnail) -> napi::Result<()> {
    self
      .display_updater()?
      .SetThumbnail(&thumbnail.stream_ref)
      .map_err(map_windows_error)
  }

  /// Sets the timeline data
  ///
  /// You MUST call this function everytime the position changes in the song. The media service will become out of sync if this is not called enough or cause seeked signals to be emitted to the media service unnecessarily.
  #[napi]
  #[allow(dead_code)]
  pub fn set_timeline(&mut self, duration: f64, position: f64) -> napi::Result<()> {
    validate_timeline(duration, position)?;

    let smtc = self.smtc()?;
    let timeline_props =
      SystemMediaTransportControlsTimelineProperties::new().map_err(map_windows_error)?;

    timeline_props
      .SetStartTime(secs_to_timespan(0.0))
      .map_err(map_windows_error)?;
    timeline_props
      .SetEndTime(secs_to_timespan(duration))
      .map_err(map_windows_error)?;
    timeline_props
      .SetPosition(secs_to_timespan(position))
      .map_err(map_windows_error)?;
    timeline_props
      .SetMinSeekTime(secs_to_timespan(0.0))
      .map_err(map_windows_error)?;
    timeline_props
      .SetMaxSeekTime(secs_to_timespan(duration))
      .map_err(map_windows_error)?;

    smtc
      .UpdateTimelineProperties(&timeline_props)
      .map_err(map_windows_error)
  }

  /// Gets the play button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_play_button_enabled(&self) -> napi::Result<bool> {
    self.smtc()?.IsPlayEnabled().map_err(map_windows_error)
  }

  /// Sets the play button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_play_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self
      .smtc()?
      .SetIsPlayEnabled(enabled)
      .map_err(map_windows_error)
  }

  /// Gets the paused button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_pause_button_enabled(&self) -> napi::Result<bool> {
    self.smtc()?.IsPauseEnabled().map_err(map_windows_error)
  }

  /// Sets the paused button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_pause_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self
      .smtc()?
      .SetIsPauseEnabled(enabled)
      .map_err(map_windows_error)
  }

  /// Gets the stop button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_stop_button_enabled(&self) -> napi::Result<bool> {
    self.smtc()?.IsStopEnabled().map_err(map_windows_error)
  }

  /// Sets the stop button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_stop_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self
      .smtc()?
      .SetIsStopEnabled(enabled)
      .map_err(map_windows_error)
  }

  /// Gets the previous button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_previous_button_enabled(&self) -> napi::Result<bool> {
    self.smtc()?.IsPreviousEnabled().map_err(map_windows_error)
  }

  /// Sets the previous button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_previous_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self
      .smtc()?
      .SetIsPreviousEnabled(enabled)
      .map_err(map_windows_error)
  }

  /// Gets the next button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_next_button_enabled(&self) -> napi::Result<bool> {
    self.smtc()?.IsNextEnabled().map_err(map_windows_error)
  }

  /// Sets the next button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_next_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self
      .smtc()?
      .SetIsNextEnabled(enabled)
      .map_err(map_windows_error)
  }

  /// Gets the seek enabled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_seek_enabled(&self) -> napi::Result<bool> {
    Ok(self.seek_enabled.load(Ordering::Relaxed))
  }

  /// Sets the seek enabled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_seek_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self.seek_enabled.store(enabled, Ordering::Relaxed);
    Ok(())
  }

  /// Gets the playback rate
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_playback_rate(&self) -> napi::Result<f64> {
    self.smtc()?.PlaybackRate().map_err(map_windows_error)
  }

  /// Sets the playback rate
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_playback_rate(&mut self, playback_rate: f64) -> napi::Result<()> {
    self
      .smtc()?
      .SetPlaybackRate(playback_rate)
      .map_err(map_windows_error)
  }

  /// Gets the playback status
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_playback_status(&self) -> napi::Result<MediaPlayerPlaybackStatus> {
    let playback_status = self.smtc()?.PlaybackStatus().map_err(map_windows_error)?;
    Ok(match playback_status {
      MediaPlaybackStatus::Playing => MediaPlayerPlaybackStatus::Playing,
      MediaPlaybackStatus::Paused => MediaPlayerPlaybackStatus::Paused,
      MediaPlaybackStatus::Stopped => MediaPlayerPlaybackStatus::Stopped,
      _ => MediaPlayerPlaybackStatus::Unknown,
    })
  }

  /// Sets the playback status
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_playback_status(
    &mut self,
    playback_status: MediaPlayerPlaybackStatus,
  ) -> napi::Result<()> {
    let status = match playback_status {
      MediaPlayerPlaybackStatus::Playing => MediaPlaybackStatus::Playing,
      MediaPlayerPlaybackStatus::Paused => MediaPlaybackStatus::Paused,
      MediaPlayerPlaybackStatus::Stopped => MediaPlaybackStatus::Stopped,
      _ => {
        return Err(napi::Error::from_reason(format!(
          "{:?} is not a valid MediaPlayerPlaybackStatus to set",
          playback_status
        )))
      }
    };

    self
      .smtc()?
      .SetPlaybackStatus(status)
      .map_err(map_windows_error)
  }

  /// Gets the media type
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_media_type(&self) -> napi::Result<MediaPlayerMediaType> {
    let media_type = self.display_updater()?.Type().map_err(map_windows_error)?;
    Ok(match media_type {
      MediaPlaybackType::Music => MediaPlayerMediaType::Music,
      _ => MediaPlayerMediaType::Unknown,
    })
  }

  /// Sets the media type
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_media_type(&mut self, media_type: MediaPlayerMediaType) -> napi::Result<()> {
    let playback_type = match media_type {
      MediaPlayerMediaType::Music => MediaPlaybackType::Music,
      _ => {
        return Err(napi::Error::from_reason(format!(
          "{:?} is not a valid MediaPlayerMediaType to set",
          media_type
        )))
      }
    };

    self
      .display_updater()?
      .SetType(playback_type)
      .map_err(map_windows_error)
  }

  /// Gets the media title
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_title(&self) -> napi::Result<String> {
    Ok(
      self
        .music_properties()?
        .Title()
        .map_err(map_windows_error)?
        .to_string(),
    )
  }

  /// Sets the media title
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_title(&mut self, title: String) -> napi::Result<()> {
    self
      .music_properties()?
      .SetTitle(&HSTRING::from(title))
      .map_err(map_windows_error)
  }

  /// Gets the media artist
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_artist(&self) -> napi::Result<String> {
    Ok(
      self
        .music_properties()?
        .Artist()
        .map_err(map_windows_error)?
        .to_string(),
    )
  }

  /// Sets the media artist
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_artist(&mut self, artist: String) -> napi::Result<()> {
    self
      .music_properties()?
      .SetArtist(&HSTRING::from(artist))
      .map_err(map_windows_error)
  }

  /// Gets the media album title
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_album_title(&self) -> napi::Result<String> {
    Ok(
      self
        .music_properties()?
        .AlbumTitle()
        .map_err(map_windows_error)?
        .to_string(),
    )
  }

  /// Sets the media album title
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_album_title(&mut self, album_title: String) -> napi::Result<()> {
    self
      .music_properties()?
      .SetAlbumTitle(&HSTRING::from(album_title))
      .map_err(map_windows_error)
  }

  /// Gets the track id
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_track_id(&self) -> napi::Result<String> {
    Ok(self.track_id.clone())
  }

  /// Sets the track id
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_track_id(&mut self, track_id: String) -> napi::Result<()> {
    self.track_id = track_id;
    Ok(())
  }

  fn smtc(&self) -> napi::Result<SystemMediaTransportControls> {
    self
      .player
      .SystemMediaTransportControls()
      .map_err(map_windows_error)
  }

  fn display_updater(&self) -> napi::Result<SystemMediaTransportControlsDisplayUpdater> {
    self.smtc()?.DisplayUpdater().map_err(map_windows_error)
  }

  fn music_properties(&self) -> napi::Result<MusicDisplayProperties> {
    self
      .display_updater()?
      .MusicProperties()
      .map_err(map_windows_error)
  }
}

impl ObjectFinalize for MediaPlayer {
  fn finalize(self, _env: napi::Env) -> napi::Result<()> {
    // Best-effort cleanup so one SMTC unregister failure does not skip Close().
    if let Ok(smtc) = self.smtc() {
      let _ = smtc.RemoveButtonPressed(self.smtc_button_pressed_registration);
      let _ = smtc
        .RemovePlaybackPositionChangeRequested(self.smtc_playback_position_changed_registration);
    }

    self.button_pressed_listeners.clear();
    self.playback_position_changed_listeners.clear();
    self.playback_position_seeked_listeners.clear();

    self.player.Close().map_err(map_windows_error)
  }
}

fn dispatch_smtc_button(
  button: SystemMediaTransportControlsButton,
  button_listeners: &ButtonListeners,
  seeked_listeners: &PositionListeners,
  seek_enabled: bool,
) {
  match button {
    SystemMediaTransportControlsButton::Play => emit_string(button_listeners, "play"),
    SystemMediaTransportControlsButton::Pause => emit_string(button_listeners, "pause"),
    SystemMediaTransportControlsButton::Stop => emit_string(button_listeners, "stop"),
    SystemMediaTransportControlsButton::Next => emit_string(button_listeners, "next"),
    SystemMediaTransportControlsButton::Previous => emit_string(button_listeners, "previous"),
    SystemMediaTransportControlsButton::FastForward if seek_enabled => {
      emit_f64(seeked_listeners, DEFAULT_SEEK_STEP_SECS)
    }
    SystemMediaTransportControlsButton::Rewind if seek_enabled => {
      emit_f64(seeked_listeners, -DEFAULT_SEEK_STEP_SECS)
    }
    _ => {}
  }
}

fn secs_to_timespan(seconds: f64) -> TimeSpan {
  TimeSpan::from(Duration::from_secs_f64(seconds))
}

fn validate_timeline(duration: f64, position: f64) -> napi::Result<()> {
  if duration < 0.0 {
    return Err(napi::Error::from_reason("Duration cannot be less than 0"));
  }
  if position < 0.0 {
    return Err(napi::Error::from_reason("Position cannot be less than 0"));
  }
  if position > duration {
    return Err(napi::Error::from_reason(
      "Position cannot be greather than provided duration",
    ));
  }
  Ok(())
}

fn map_windows_error(error: windows::core::Error) -> napi::Error {
  napi::Error::from_reason(error.message())
}

fn insert_string_listener(
  listeners: &ButtonListeners,
  env: Env,
  callback_ptr: usize,
  callback: JsFunction,
) -> napi::Result<()> {
  if listeners.contains_key(&callback_ptr) {
    return Ok(());
  }

  let mut threadsafe_callback = callback.create_threadsafe_function(0, |ctx| {
    ctx.env.create_string_from_std(ctx.value).map(|v| vec![v])
  })?;
  let _ = threadsafe_callback.unref(&env)?;
  listeners.insert(callback_ptr, threadsafe_callback);
  Ok(())
}

fn insert_f64_listener(
  listeners: &PositionListeners,
  env: Env,
  callback_ptr: usize,
  callback: JsFunction,
) -> napi::Result<()> {
  if listeners.contains_key(&callback_ptr) {
    return Ok(());
  }

  let mut threadsafe_callback =
    callback.create_threadsafe_function(0, |ctx| ctx.env.create_double(ctx.value).map(|v| vec![v]))?;
  let _ = threadsafe_callback.unref(&env)?;
  listeners.insert(callback_ptr, threadsafe_callback);
  Ok(())
}

fn emit_string(listeners: &ButtonListeners, value: &str) {
  for listener in listeners.iter() {
    listener.call(
      Ok(value.to_string()),
      ThreadsafeFunctionCallMode::NonBlocking,
    );
  }
}

fn emit_f64(listeners: &PositionListeners, value: f64) {
  for listener in listeners.iter() {
    listener.call(Ok(value), ThreadsafeFunctionCallMode::NonBlocking);
  }
}

#[cfg(test)]
mod tests {
  use super::{secs_to_timespan, validate_timeline, DEFAULT_SEEK_STEP_SECS};
  use std::time::Duration;
  use windows::Foundation::TimeSpan;

  #[test]
  fn secs_to_timespan_roundtrips_duration() {
    let span: TimeSpan = secs_to_timespan(12.5);
    assert_eq!(Duration::from(span), Duration::from_secs_f64(12.5));
  }

  #[test]
  fn default_seek_step_is_positive() {
    assert!(DEFAULT_SEEK_STEP_SECS > 0.0);
  }

  #[test]
  fn timeline_validation_rejects_invalid_ranges() {
    assert!(validate_timeline(-1.0, 0.0).is_err());
    assert!(validate_timeline(10.0, -1.0).is_err());
    assert!(validate_timeline(10.0, 11.0).is_err());
    assert!(validate_timeline(10.0, 10.0).is_ok());
  }
}

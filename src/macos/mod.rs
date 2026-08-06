use std::{
  ffi::c_void,
  sync::{Arc, RwLock},
  time::Duration,
};

use dashmap::DashMap;
use napi::{
  bindgen_prelude::ObjectFinalize,
  threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode},
  Env, JsFunction, NapiRaw,
};
use souvlaki::{
  MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
  SeekDirection,
};

/// Default relative seek amount when macOS sends a direction-only seek command.
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
  thumbnail: String,
}

#[napi]
impl MediaPlayerThumbnail {
  #[napi(factory)]
  #[allow(dead_code)]
  pub async fn create(
    thumbnail_type: MediaPlayerThumbnailType,
    thumbnail: String,
  ) -> napi::Result<Self> {
    match thumbnail_type {
      MediaPlayerThumbnailType::File => Ok(Self {
        thumbnail_type,
        thumbnail: format!("file://{}", thumbnail),
      }),
      MediaPlayerThumbnailType::Uri => Ok(Self {
        thumbnail_type,
        thumbnail,
      }),
      _ => Err(napi::Error::from_reason(format!(
        "{:?} is not a valid MediaPlayerThumbnailType to create",
        thumbnail_type
      ))),
    }
  }

  #[napi(getter, js_name = "type")]
  #[allow(dead_code)]
  pub fn thumbnail_type(&self) -> MediaPlayerThumbnailType {
    self.thumbnail_type
  }
}

#[derive(Clone, Debug, PartialEq)]
struct MetadataSnapshot {
  title: String,
  album_title: String,
  artist: String,
  thumbnail: String,
  duration: f64,
}

#[derive(Clone, Debug, PartialEq)]
struct PlaybackSnapshot {
  playback_status: MediaPlayerPlaybackStatus,
  position: f64,
}

#[derive(Clone, Debug)]
struct FlushPayload {
  state_revision: u64,
  metadata: Option<MetadataSnapshot>,
  playback: Option<PlaybackSnapshot>,
}

#[derive(Clone, Copy)]
enum FlushMode {
  None,
  Full,
  TrackChange,
  MetadataOnly,
  PlaybackOnly,
}

#[derive(Default)]
struct TitleDataPatch {
  title: Option<String>,
  artist: Option<String>,
  album_title: Option<String>,
  thumbnail: Option<String>,
  track_id: Option<String>,
}

#[derive(Default)]
struct PlaybackStatePatch {
  duration: Option<f64>,
  position: Option<f64>,
  playback_status: Option<MediaPlayerPlaybackStatus>,
}

struct PlaybackPatchResult {
  changed: bool,
  completed_track_transition: bool,
}

#[derive(Debug)]
struct MediaPlayerState {
  active: bool,
  can_go_next: bool,
  can_go_previous: bool,
  can_play: bool,
  can_pause: bool,
  can_seek: bool,
  can_control: bool,
  media_type: MediaPlayerMediaType,
  playback_status: MediaPlayerPlaybackStatus,
  thumbnail: String,
  artist: String,
  album_title: String,
  title: String,
  track_id: String,
  duration: f64,
  position: f64,
  playback_rate: f64,
  state_revision: u64,
  track_revision: u64,
  /// Track revision that may accept `SetPosition` events from the OS.
  position_event_track_revision: u64,
  track_transition_pending: bool,
  /// Avoid Now Playing position jumps when only status changes (e.g. pause).
  prefer_last_playback_position_for_status_flush: bool,
  metadata_dirty: bool,
  playback_dirty: bool,
  last_metadata_snapshot: Option<MetadataSnapshot>,
  last_playback_snapshot: Option<PlaybackSnapshot>,
}

impl Default for MediaPlayerState {
  fn default() -> Self {
    Self {
      active: false,
      can_go_next: false,
      can_go_previous: false,
      can_play: false,
      can_pause: false,
      can_seek: false,
      can_control: true,
      media_type: MediaPlayerMediaType::Unknown,
      playback_status: MediaPlayerPlaybackStatus::Unknown,
      thumbnail: String::new(),
      artist: String::new(),
      album_title: String::new(),
      title: String::new(),
      track_id: String::new(),
      duration: 0.0,
      position: 0.0,
      playback_rate: 1.0,
      state_revision: 0,
      track_revision: 0,
      position_event_track_revision: 0,
      track_transition_pending: false,
      prefer_last_playback_position_for_status_flush: false,
      metadata_dirty: false,
      playback_dirty: false,
      last_metadata_snapshot: None,
      last_playback_snapshot: None,
    }
  }
}

impl MediaPlayerState {
  fn bump_revision(&mut self) {
    self.state_revision = self.state_revision.saturating_add(1);
  }

  fn apply_title_patch(&mut self, patch: TitleDataPatch) -> bool {
    let mut changed = false;

    changed |= assign_if_changed(&mut self.title, patch.title);
    changed |= assign_if_changed(&mut self.artist, patch.artist);
    changed |= assign_if_changed(&mut self.album_title, patch.album_title);
    changed |= assign_if_changed(&mut self.thumbnail, patch.thumbnail);

    if let Some(track_id) = patch.track_id {
      if self.track_id != track_id {
        self.track_id = track_id;
        self.track_revision = self.track_revision.saturating_add(1);
        self.track_transition_pending = true;
        self.duration = 0.0;
        self.position = 0.0;
        self.playback_dirty = true;
        self.prefer_last_playback_position_for_status_flush = false;
        changed = true;
      }
    }

    if changed {
      self.metadata_dirty = true;
      self.bump_revision();
    }

    changed
  }

  fn apply_playback_patch(&mut self, patch: PlaybackStatePatch) -> PlaybackPatchResult {
    let mut playback_changed = false;
    let mut duration_changed = false;
    let mut playback_status_changed = false;
    let mut completed_track_transition = false;

    if let Some(duration) = patch.duration {
      if (self.duration - duration).abs() > f64::EPSILON {
        self.duration = duration;
        duration_changed = true;
      }
    }

    if let Some(position) = patch.position {
      if (self.position - position).abs() > f64::EPSILON {
        self.position = position;
        playback_changed = true;
      }
    }

    if let Some(playback_status) = patch.playback_status {
      if self.playback_status != playback_status {
        self.playback_status = playback_status;
        playback_status_changed = true;
        playback_changed = true;
      }
    }

    if duration_changed {
      self.metadata_dirty = true;
      playback_changed = true;
    }

    if self.track_transition_pending && (duration_changed || patch.position.is_some()) {
      self.position_event_track_revision = self.track_revision;
      self.track_transition_pending = false;
      completed_track_transition = true;
    }

    if patch.position.is_some() || duration_changed {
      self.prefer_last_playback_position_for_status_flush = false;
    } else if playback_status_changed {
      self.prefer_last_playback_position_for_status_flush = true;
    }

    if playback_changed {
      self.playback_dirty = true;
      self.bump_revision();
    }

    PlaybackPatchResult {
      changed: playback_changed,
      completed_track_transition,
    }
  }

  fn metadata_snapshot(&self) -> MetadataSnapshot {
    MetadataSnapshot {
      title: self.title.clone(),
      album_title: self.album_title.clone(),
      artist: self.artist.clone(),
      thumbnail: self.thumbnail.clone(),
      duration: self.duration.max(0.0),
    }
  }

  fn playback_snapshot(&self) -> PlaybackSnapshot {
    let position = if self.prefer_last_playback_position_for_status_flush
      && !self.track_transition_pending
    {
      self
        .last_playback_snapshot
        .as_ref()
        .map_or(self.position.max(0.0), |snapshot| snapshot.position.max(0.0))
    } else {
      self.position.max(0.0)
    };

    PlaybackSnapshot {
      playback_status: self.playback_status,
      position,
    }
  }

  fn should_emit_metadata(&self, snapshot: &MetadataSnapshot, flush_mode: FlushMode) -> bool {
    matches!(
      flush_mode,
      FlushMode::Full | FlushMode::TrackChange | FlushMode::MetadataOnly
    ) && (self.metadata_dirty || self.last_metadata_snapshot.as_ref() != Some(snapshot))
  }

  fn should_emit_playback(&self, snapshot: &PlaybackSnapshot, flush_mode: FlushMode) -> bool {
    matches!(
      flush_mode,
      FlushMode::Full | FlushMode::TrackChange | FlushMode::PlaybackOnly
    ) && (self.playback_dirty || self.last_playback_snapshot.as_ref() != Some(snapshot))
  }

  fn create_flush_payload(&self, flush_mode: FlushMode) -> Option<FlushPayload> {
    if !self.active || matches!(flush_mode, FlushMode::None) {
      return None;
    }

    let metadata_snapshot = self.metadata_snapshot();
    let playback_snapshot = self.playback_snapshot();
    let emit_metadata = self.should_emit_metadata(&metadata_snapshot, flush_mode);
    let emit_playback = self.should_emit_playback(&playback_snapshot, flush_mode);

    if !emit_metadata && !emit_playback {
      return None;
    }

    Some(FlushPayload {
      state_revision: self.state_revision,
      metadata: emit_metadata.then_some(metadata_snapshot),
      playback: emit_playback.then_some(playback_snapshot),
    })
  }

  fn mark_metadata_flushed(&mut self, state_revision: u64, snapshot: MetadataSnapshot) {
    self.last_metadata_snapshot = Some(snapshot);
    if self.state_revision == state_revision {
      self.metadata_dirty = false;
    }
  }

  fn mark_playback_flushed(&mut self, state_revision: u64, snapshot: PlaybackSnapshot) {
    self.last_playback_snapshot = Some(snapshot);
    if self.state_revision == state_revision {
      self.playback_dirty = false;
      self.prefer_last_playback_position_for_status_flush = false;
    }
  }

  fn accepts_set_position(&self, requested_seconds: f64) -> bool {
    self.can_seek
      && requested_seconds <= self.duration
      && self.position_event_track_revision == self.track_revision
  }
}

#[napi(custom_finalize)]
struct MediaPlayer {
  media_controls: MediaControls,
  button_pressed_listeners: ButtonListeners,
  playback_position_changed_listeners: PositionListeners,
  playback_position_seeked_listeners: PositionListeners,
  state: Arc<RwLock<MediaPlayerState>>,
}

#[napi]
impl MediaPlayer {
  #[napi(constructor)]
  #[allow(dead_code)]
  pub fn new(service_name: String, identity: String) -> napi::Result<Self> {
    let button_pressed_listeners = Arc::new(DashMap::new());
    let playback_position_changed_listeners = Arc::new(DashMap::new());
    let playback_position_seeked_listeners = Arc::new(DashMap::new());
    let state = Arc::new(RwLock::new(MediaPlayerState::default()));

    let mut media_controls = MediaControls::new(PlatformConfig {
      display_name: &identity,
      dbus_name: &service_name,
      hwnd: Option::<*mut c_void>::None,
    })
    .map_err(map_souvlaki_error)?;

    let event_state = state.clone();
    let event_buttons = button_pressed_listeners.clone();
    let event_position_changed = playback_position_changed_listeners.clone();
    let event_position_seeked = playback_position_seeked_listeners.clone();

    media_controls
      .attach(move |event: MediaControlEvent| {
        handle_media_control_event(
          event,
          &event_state,
          &event_buttons,
          &event_position_changed,
          &event_position_seeked,
        );
      })
      .map_err(map_souvlaki_error)?;

    Ok(Self {
      media_controls,
      button_pressed_listeners,
      playback_position_changed_listeners,
      playback_position_seeked_listeners,
      state,
    })
  }

  /// Activates the MediaPlayer allowing the operating system to see and use it
  #[napi]
  #[allow(dead_code)]
  pub fn activate(&mut self) -> napi::Result<()> {
    self.with_state_mut(|state| state.active = true);
    self.flush_state(FlushMode::Full)
  }

  /// Deactivates the MediaPlayer denying the operating system to see and use it
  #[napi]
  #[allow(dead_code)]
  pub fn deactivate(&mut self) -> napi::Result<()> {
    self.with_state_mut(|state| state.active = false);
    self
      .media_controls
      .set_playback(MediaPlayback::Stopped)
      .map_err(map_souvlaki_error)
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
      "buttonpressed" => insert_string_listener(&self.button_pressed_listeners, env, callback_ptr, callback)?,
      "positionchanged" => {
        insert_f64_listener(&self.playback_position_changed_listeners, env, callback_ptr, callback)?
      }
      "positionseeked" => {
        insert_f64_listener(&self.playback_position_seeked_listeners, env, callback_ptr, callback)?
      }
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
        self.playback_position_changed_listeners.remove(&callback_ptr);
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
  pub fn update(&mut self) -> napi::Result<()> {
    self.flush_state(FlushMode::Full)
  }

  /// Sets the thumbnail
  #[napi]
  #[allow(dead_code)]
  pub fn set_thumbnail(&mut self, thumbnail: &MediaPlayerThumbnail) -> napi::Result<()> {
    self.update_title_data(
      TitleDataPatch {
        thumbnail: Some(thumbnail.thumbnail.clone()),
        ..TitleDataPatch::default()
      },
      FlushMode::MetadataOnly,
    )
  }

  /// Sets the timeline data
  ///
  /// You MUST call this function everytime the position changes in the song. The media service will become out of sync if this is not called enough or cause seeked signals to be emitted to the media service unnecessarily.
  #[napi]
  #[allow(dead_code)]
  pub fn set_timeline(&mut self, duration: f64, position: f64) -> napi::Result<()> {
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

    let patch_result = self.update_playback_state(PlaybackStatePatch {
      duration: Some(duration),
      position: Some(position),
      ..PlaybackStatePatch::default()
    });

    if !patch_result.changed {
      return Ok(());
    }

    let flush_mode = if patch_result.completed_track_transition {
      FlushMode::TrackChange
    } else {
      FlushMode::PlaybackOnly
    };

    self.flush_state(flush_mode)
  }

  /// Gets the play button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_play_button_enabled(&self) -> napi::Result<bool> {
    Ok(self.read_state(|state| state.can_play, false))
  }

  /// Sets the play button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_play_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self.with_state_mut(|state| state.can_play = enabled);
    Ok(())
  }

  /// Gets the paused button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_pause_button_enabled(&self) -> napi::Result<bool> {
    Ok(self.read_state(|state| state.can_pause, false))
  }

  /// Sets the paused button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_pause_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self.with_state_mut(|state| state.can_pause = enabled);
    Ok(())
  }

  /// Gets the paused button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_stop_button_enabled(&self) -> napi::Result<bool> {
    Ok(self.read_state(|state| state.can_control, false))
  }

  /// Sets the paused button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_stop_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self.with_state_mut(|state| state.can_control = enabled);
    Ok(())
  }

  /// Gets the previous button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_previous_button_enabled(&self) -> napi::Result<bool> {
    Ok(self.read_state(|state| state.can_go_previous, false))
  }

  /// Sets the previous button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_previous_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self.with_state_mut(|state| state.can_go_previous = enabled);
    Ok(())
  }

  /// Gets the next button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_next_button_enabled(&self) -> napi::Result<bool> {
    Ok(self.read_state(|state| state.can_go_next, false))
  }

  /// Sets the next button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_next_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self.with_state_mut(|state| state.can_go_next = enabled);
    Ok(())
  }

  /// Gets the seek enabled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_seek_enabled(&self) -> napi::Result<bool> {
    Ok(self.read_state(|state| state.can_seek, false))
  }

  /// Sets the seek enabled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_seek_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    self.with_state_mut(|state| state.can_seek = enabled);
    Ok(())
  }

  /// Gets the playback rate
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_playback_rate(&self) -> napi::Result<f64> {
    Ok(self.read_state(|state| state.playback_rate, 1.0))
  }

  /// Sets the playback rate
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_playback_rate(&mut self, playback_rate: f64) -> napi::Result<()> {
    self.with_state_mut(|state| state.playback_rate = playback_rate);
    Ok(())
  }

  /// Gets the playback status
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_playback_status(&self) -> napi::Result<MediaPlayerPlaybackStatus> {
    Ok(self.read_state(
      |state| state.playback_status,
      MediaPlayerPlaybackStatus::Unknown,
    ))
  }

  /// Sets the playback status
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_playback_status(
    &mut self,
    playback_status: MediaPlayerPlaybackStatus,
  ) -> napi::Result<()> {
    if playback_status == MediaPlayerPlaybackStatus::Unknown {
      return Err(napi::Error::from_reason(format!(
        "{:?} is not a valid MediaPlayerPlaybackStatus to set",
        playback_status
      )));
    }

    let patch_result = self.update_playback_state(PlaybackStatePatch {
      playback_status: Some(playback_status),
      ..PlaybackStatePatch::default()
    });
    if !patch_result.changed {
      return Ok(());
    }
    self.flush_state(FlushMode::PlaybackOnly)
  }

  /// Gets the media type
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_media_type(&self) -> napi::Result<MediaPlayerMediaType> {
    Ok(self.read_state(|state| state.media_type, MediaPlayerMediaType::Unknown))
  }

  /// Sets the media type
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_media_type(&mut self, media_type: MediaPlayerMediaType) -> napi::Result<()> {
    if media_type == MediaPlayerMediaType::Unknown {
      return Err(napi::Error::from_reason(format!(
        "{:?} is not a valid MediaPlayerMediaType to set",
        media_type
      )));
    }

    self.with_state_mut(|state| state.media_type = media_type);
    Ok(())
  }

  /// Gets the media title
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_title(&self) -> napi::Result<String> {
    Ok(self.read_state(|state| state.title.clone(), String::new()))
  }

  /// Sets the media title
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_title(&mut self, title: String) -> napi::Result<()> {
    self.update_title_data(
      TitleDataPatch {
        title: Some(title),
        ..TitleDataPatch::default()
      },
      FlushMode::MetadataOnly,
    )
  }

  /// Gets the media artist
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_artist(&self) -> napi::Result<String> {
    Ok(self.read_state(|state| state.artist.clone(), String::new()))
  }

  /// Sets the media artist
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_artist(&mut self, artist: String) -> napi::Result<()> {
    self.update_title_data(
      TitleDataPatch {
        artist: Some(artist),
        ..TitleDataPatch::default()
      },
      FlushMode::MetadataOnly,
    )
  }

  /// Gets the media album title
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_album_title(&self) -> napi::Result<String> {
    Ok(self.read_state(|state| state.album_title.clone(), String::new()))
  }

  /// Sets the media artist
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_album_title(&mut self, album_title: String) -> napi::Result<()> {
    self.update_title_data(
      TitleDataPatch {
        album_title: Some(album_title),
        ..TitleDataPatch::default()
      },
      FlushMode::MetadataOnly,
    )
  }

  /// Gets the track id
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_track_id(&self) -> napi::Result<String> {
    Ok(self.read_state(|state| state.track_id.clone(), String::new()))
  }

  /// Sets the track id
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_track_id(&mut self, track_id: String) -> napi::Result<()> {
    self.update_title_data(
      TitleDataPatch {
        track_id: Some(track_id),
        ..TitleDataPatch::default()
      },
      FlushMode::None,
    )
  }

  fn read_state<T>(&self, mapper: impl FnOnce(&MediaPlayerState) -> T, fallback: T) -> T {
    self
      .state
      .read()
      .map(|state| mapper(&state))
      .unwrap_or(fallback)
  }

  fn with_state_mut(&self, mapper: impl FnOnce(&mut MediaPlayerState)) {
    if let Ok(mut state) = self.state.write() {
      mapper(&mut state);
    }
  }

  fn update_title_data(&mut self, patch: TitleDataPatch, flush_mode: FlushMode) -> napi::Result<()> {
    let changed = self
      .state
      .write()
      .map(|mut state| state.apply_title_patch(patch))
      .unwrap_or(false);

    if !changed {
      return Ok(());
    }

    self.flush_state(flush_mode)
  }

  fn update_playback_state(&mut self, patch: PlaybackStatePatch) -> PlaybackPatchResult {
    self
      .state
      .write()
      .map(|mut state| state.apply_playback_patch(patch))
      .unwrap_or(PlaybackPatchResult {
        changed: false,
        completed_track_transition: false,
      })
  }

  fn flush_state(&mut self, flush_mode: FlushMode) -> napi::Result<()> {
    let Some(payload) = self
      .state
      .read()
      .ok()
      .and_then(|state| state.create_flush_payload(flush_mode))
    else {
      return Ok(());
    };

    if let Some(metadata_snapshot) = payload.metadata.clone() {
      self.send_metadata(&metadata_snapshot)?;
      self.with_state_mut(|state| {
        state.mark_metadata_flushed(payload.state_revision, metadata_snapshot);
      });
    }

    if let Some(playback_snapshot) = payload.playback.clone() {
      self.send_playback(&playback_snapshot)?;
      self.with_state_mut(|state| {
        state.mark_playback_flushed(payload.state_revision, playback_snapshot);
      });
    }

    Ok(())
  }

  fn send_metadata(&mut self, metadata_snapshot: &MetadataSnapshot) -> napi::Result<()> {
    let metadata = MediaMetadata {
      title: to_optional_ref(metadata_snapshot.title.as_str()),
      album: to_optional_ref(metadata_snapshot.album_title.as_str()),
      artist: to_optional_ref(metadata_snapshot.artist.as_str()),
      cover_url: to_optional_ref(metadata_snapshot.thumbnail.as_str()),
      duration: Some(Duration::from_secs_f64(metadata_snapshot.duration)),
    };

    self
      .media_controls
      .set_metadata(metadata)
      .map_err(map_souvlaki_error)
  }

  fn send_playback(&mut self, playback_snapshot: &PlaybackSnapshot) -> napi::Result<()> {
    let progress = Some(MediaPosition(Duration::from_secs_f64(
      playback_snapshot.position,
    )));
    let playback = match playback_snapshot.playback_status {
      MediaPlayerPlaybackStatus::Playing => MediaPlayback::Playing { progress },
      MediaPlayerPlaybackStatus::Paused => MediaPlayback::Paused { progress },
      _ => MediaPlayback::Stopped,
    };

    self
      .media_controls
      .set_playback(playback)
      .map_err(map_souvlaki_error)
  }
}

impl ObjectFinalize for MediaPlayer {
  fn finalize(mut self, _env: napi::Env) -> napi::Result<()> {
    let _ = self.media_controls.detach();
    self.button_pressed_listeners.clear();
    self.playback_position_changed_listeners.clear();
    self.playback_position_seeked_listeners.clear();
    Ok(())
  }
}

fn handle_media_control_event(
  event: MediaControlEvent,
  state: &Arc<RwLock<MediaPlayerState>>,
  button_pressed_listeners: &ButtonListeners,
  playback_position_changed_listeners: &PositionListeners,
  playback_position_seeked_listeners: &PositionListeners,
) {
  let Ok(current_state) = state.read() else {
    return;
  };

  if !current_state.active {
    return;
  }

  match event {
    MediaControlEvent::Play if current_state.can_play => {
      emit_string(button_pressed_listeners, "play");
    }
    MediaControlEvent::Pause if current_state.can_pause => {
      emit_string(button_pressed_listeners, "pause");
    }
    MediaControlEvent::Toggle if current_state.can_play || current_state.can_pause => {
      emit_string(button_pressed_listeners, "playpause");
    }
    MediaControlEvent::Next if current_state.can_go_next => {
      emit_string(button_pressed_listeners, "next");
    }
    MediaControlEvent::Previous if current_state.can_go_previous => {
      emit_string(button_pressed_listeners, "previous");
    }
    MediaControlEvent::Stop if current_state.can_control => {
      emit_string(button_pressed_listeners, "stop");
    }
    MediaControlEvent::Seek(direction) if current_state.can_seek => {
      emit_f64(
        playback_position_seeked_listeners,
        signed_seek_seconds(direction, DEFAULT_SEEK_STEP_SECS),
      );
    }
    MediaControlEvent::SeekBy(direction, amount) if current_state.can_seek => {
      emit_f64(
        playback_position_seeked_listeners,
        signed_seek_seconds(direction, amount.as_secs_f64()),
      );
    }
    MediaControlEvent::SetPosition(position) => {
      let requested_seconds = position.0.as_secs_f64();
      if current_state.accepts_set_position(requested_seconds) {
        emit_f64(playback_position_changed_listeners, requested_seconds);
      }
    }
    _ => {}
  }
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

  let mut threadsafe_callback =
    callback.create_threadsafe_function(0, |ctx| {
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

fn signed_seek_seconds(direction: SeekDirection, amount: f64) -> f64 {
  match direction {
    SeekDirection::Forward => amount,
    SeekDirection::Backward => -amount,
  }
}

fn assign_if_changed(target: &mut String, next: Option<String>) -> bool {
  let Some(next) = next else {
    return false;
  };
  if *target == next {
    return false;
  }
  *target = next;
  true
}

fn to_optional_ref(value: &str) -> Option<&str> {
  if value.is_empty() {
    None
  } else {
    Some(value)
  }
}

fn map_souvlaki_error(error: souvlaki::Error) -> napi::Error {
  napi::Error::from_reason(format!("{:?}", error))
}

#[cfg(test)]
mod tests {
  use super::{
    FlushMode, MediaPlayerPlaybackStatus, MediaPlayerState, PlaybackStatePatch, TitleDataPatch,
  };

  fn build_test_state() -> MediaPlayerState {
    MediaPlayerState {
      active: true,
      can_go_next: true,
      can_go_previous: true,
      can_play: true,
      can_pause: true,
      can_seek: true,
      can_control: true,
      media_type: super::MediaPlayerMediaType::Music,
      playback_status: MediaPlayerPlaybackStatus::Paused,
      ..MediaPlayerState::default()
    }
  }

  #[test]
  fn track_transition_is_completed_by_timeline_patch() {
    let mut state = build_test_state();

    assert!(state.apply_title_patch(TitleDataPatch {
      track_id: Some(String::from("track-2")),
      ..TitleDataPatch::default()
    }));
    assert!(state.track_transition_pending);
    assert_eq!(state.track_revision, 1);
    assert_eq!(state.position_event_track_revision, 0);

    let playback_result = state.apply_playback_patch(PlaybackStatePatch {
      duration: Some(200.0),
      position: Some(0.0),
      ..PlaybackStatePatch::default()
    });

    assert!(playback_result.completed_track_transition);
    assert_eq!(state.position_event_track_revision, state.track_revision);
    assert!(!state.track_transition_pending);
  }

  #[test]
  fn set_position_is_rejected_for_stale_track_context() {
    let mut state = build_test_state();
    state.track_revision = 2;
    state.position_event_track_revision = 1;
    state.duration = 120.0;

    assert!(!state.accepts_set_position(12.0));

    state.position_event_track_revision = 2;
    assert!(state.accepts_set_position(12.0));
  }

  #[test]
  fn metadata_flush_requires_dirty_or_changed_snapshot() {
    let mut state = build_test_state();
    state.title = String::from("Song");
    state.duration = 180.0;

    let metadata_snapshot = state.metadata_snapshot();
    state.last_metadata_snapshot = Some(metadata_snapshot.clone());
    state.metadata_dirty = false;

    assert!(!state.should_emit_metadata(&metadata_snapshot, FlushMode::PlaybackOnly));
    assert!(!state.should_emit_metadata(&metadata_snapshot, FlushMode::TrackChange));

    state.metadata_dirty = true;
    assert!(state.should_emit_metadata(&metadata_snapshot, FlushMode::TrackChange));
  }

  #[test]
  fn paused_playback_flush_uses_latest_position_snapshot() {
    let mut state = build_test_state();
    state.position = 55.0;
    state.playback_status = MediaPlayerPlaybackStatus::Playing;
    state.last_playback_snapshot = Some(super::PlaybackSnapshot {
      playback_status: MediaPlayerPlaybackStatus::Playing,
      position: 58.0,
    });
    state.playback_dirty = false;

    assert!(
      state
        .apply_playback_patch(PlaybackStatePatch {
          playback_status: Some(MediaPlayerPlaybackStatus::Paused),
          ..PlaybackStatePatch::default()
        })
        .changed
    );
    assert!(state.prefer_last_playback_position_for_status_flush);

    let playback_snapshot = state.playback_snapshot();
    assert_eq!(playback_snapshot.position, 58.0);
    assert_eq!(
      playback_snapshot.playback_status,
      MediaPlayerPlaybackStatus::Paused
    );
    assert!(state.should_emit_playback(&playback_snapshot, FlushMode::PlaybackOnly));
  }

  #[test]
  fn track_change_resets_timeline_state_before_next_playback_flush() {
    let mut state = build_test_state();
    state.duration = 245.0;
    state.position = 182.0;
    state.last_playback_snapshot = Some(super::PlaybackSnapshot {
      playback_status: MediaPlayerPlaybackStatus::Playing,
      position: 182.0,
    });

    assert!(state.apply_title_patch(TitleDataPatch {
      track_id: Some(String::from("new-track")),
      ..TitleDataPatch::default()
    }));
    assert_eq!(state.duration, 0.0);
    assert_eq!(state.position, 0.0);
    assert!(state.track_transition_pending);
  }

  #[test]
  fn inactive_state_does_not_create_flush_payload() {
    let mut state = build_test_state();
    state.active = false;
    state.metadata_dirty = true;
    state.playback_dirty = true;

    assert!(state.create_flush_payload(FlushMode::Full).is_none());
  }
}

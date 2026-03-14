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
  position_event_track_revision: u64,
  track_transition_pending: bool,
  prefer_last_playback_position_for_status_flush: bool,
  metadata_dirty: bool,
  playback_dirty: bool,
  last_metadata_snapshot: Option<MetadataSnapshot>,
  last_playback_snapshot: Option<PlaybackSnapshot>,
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

#[napi(custom_finalize)]
struct MediaPlayer {
  media_controls: MediaControls,
  button_pressed_listeners:
    Arc<DashMap<usize, ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>>,
  playback_position_changed_listeners:
    Arc<DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>>,
  playback_position_seeked_listeners:
    Arc<DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>>,
  state: Arc<RwLock<MediaPlayerState>>,
}

#[napi]
impl MediaPlayer {
  #[napi(constructor)]
  #[allow(dead_code)]
  pub fn new(service_name: String, identity: String) -> napi::Result<Self> {
    let button_pressed_listeners: Arc<
      DashMap<usize, ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>,
    > = Arc::new(DashMap::new());
    let playback_position_changed_listeners: Arc<
      DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>,
    > = Arc::new(DashMap::new());
    let playback_position_seeked_listeners: Arc<
      DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>,
    > = Arc::new(DashMap::new());

    let state: Arc<RwLock<MediaPlayerState>> = Arc::new(RwLock::new(MediaPlayerState {
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
    }));

    let mut media_controls: MediaControls = MediaControls::new(PlatformConfig {
      display_name: &identity,
      dbus_name: &service_name,
      hwnd: Option::<*mut c_void>::None,
    })
    .map_err(map_souvlaki_error)?;

    let closure_state: Arc<RwLock<MediaPlayerState>> = state.clone();
    let closure_button_pressed_listeners = button_pressed_listeners.clone();
    let closure_position_changed_listeners = playback_position_changed_listeners.clone();
    let closure_position_seeked_listeners = playback_position_seeked_listeners.clone();

    media_controls
      .attach(move |event: MediaControlEvent| {
        handle_media_control_event(
          event,
          &closure_state,
          &closure_button_pressed_listeners,
          &closure_position_changed_listeners,
          &closure_position_seeked_listeners,
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
    if let Ok(mut state) = self.state.write() {
      state.active = true;
    }
    self.flush_state(FlushMode::Full)
  }

  /// Deactivates the MediaPlayer denying the operating system to see and use it
  #[napi]
  #[allow(dead_code)]
  pub fn deactivate(&mut self) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.active = false;
    }
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
    let callback_ptr: usize = unsafe { callback.raw() as usize };

    match event_name.as_str() {
      "buttonpressed" => {
        if !self.button_pressed_listeners.contains_key(&callback_ptr) {
          let mut threadsafe_callback = callback.create_threadsafe_function(0, |ctx| {
            ctx.env.create_string_from_std(ctx.value).map(|v| vec![v])
          })?;
          let _ = threadsafe_callback.unref(&env)?;
          self
            .button_pressed_listeners
            .insert(callback_ptr, threadsafe_callback);
        }
      }
      "positionchanged" => {
        if !self
          .playback_position_changed_listeners
          .contains_key(&callback_ptr)
        {
          let mut threadsafe_callback = callback.create_threadsafe_function(0, |ctx| {
            ctx.env.create_double(ctx.value).map(|v| vec![v])
          })?;
          let _ = threadsafe_callback.unref(&env)?;
          self
            .playback_position_changed_listeners
            .insert(callback_ptr, threadsafe_callback);
        }
      }
      "positionseeked" => {
        if !self
          .playback_position_seeked_listeners
          .contains_key(&callback_ptr)
        {
          let mut threadsafe_callback = callback.create_threadsafe_function(0, |ctx| {
            ctx.env.create_double(ctx.value).map(|v| vec![v])
          })?;
          let _ = threadsafe_callback.unref(&env)?;
          self
            .playback_position_seeked_listeners
            .insert(callback_ptr, threadsafe_callback);
        }
      }
      _ => {}
    };

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
    let callback_ptr: usize = unsafe { callback.raw() as usize };

    match event_name.as_str() {
      "buttonpressed" => {
        if self.button_pressed_listeners.contains_key(&callback_ptr) {
          self.button_pressed_listeners.remove(&callback_ptr);
        }
      }
      "positionchanged" => {
        if self
          .playback_position_changed_listeners
          .contains_key(&callback_ptr)
        {
          self
            .playback_position_changed_listeners
            .remove(&callback_ptr);
        }
      }
      "positionseeked" => {
        if self
          .playback_position_seeked_listeners
          .contains_key(&callback_ptr)
        {
          self
            .playback_position_seeked_listeners
            .remove(&callback_ptr);
        }
      }
      _ => {}
    };

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
        thumbnail: Some(thumbnail.thumbnail.to_owned()),
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

    let patch_result: PlaybackPatchResult = self.update_playback_state(PlaybackStatePatch {
      duration: Some(duration),
      position: Some(position),
      ..PlaybackStatePatch::default()
    });

    if !patch_result.changed {
      return Ok(());
    }

    let flush_mode: FlushMode = if patch_result.completed_track_transition {
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
    if let Ok(state) = self.state.read() {
      return Ok(state.can_play);
    }

    Ok(false)
  }

  /// Sets the play button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_play_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.can_play = enabled;
    }
    Ok(())
  }

  /// Gets the paused button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_pause_button_enabled(&self) -> napi::Result<bool> {
    if let Ok(state) = self.state.read() {
      return Ok(state.can_pause);
    }

    Ok(false)
  }

  /// Sets the paused button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_pause_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.can_pause = enabled;
    }
    Ok(())
  }

  /// Gets the paused button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_stop_button_enabled(&self) -> napi::Result<bool> {
    if let Ok(state) = self.state.read() {
      return Ok(state.can_control);
    }

    Ok(false)
  }

  /// Sets the paused button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_stop_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.can_control = enabled;
    }
    Ok(())
  }

  /// Gets the previous button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_previous_button_enabled(&self) -> napi::Result<bool> {
    if let Ok(state) = self.state.read() {
      return Ok(state.can_go_previous);
    }

    Ok(false)
  }

  /// Sets the previous button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_previous_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.can_go_previous = enabled;
    }
    Ok(())
  }

  /// Gets the next button enbled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_next_button_enabled(&self) -> napi::Result<bool> {
    if let Ok(state) = self.state.read() {
      return Ok(state.can_go_next);
    }

    Ok(false)
  }

  /// Sets the next button enbled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_next_button_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.can_go_next = enabled;
    }
    Ok(())
  }

  /// Gets the seek enabled state
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_seek_enabled(&self) -> napi::Result<bool> {
    if let Ok(state) = self.state.read() {
      return Ok(state.can_seek);
    }

    Ok(false)
  }

  /// Sets the seek enabled state
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_seek_enabled(&mut self, enabled: bool) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.can_seek = enabled;
    }
    Ok(())
  }

  /// Gets the playback rate
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_playback_rate(&self) -> napi::Result<f64> {
    if let Ok(state) = self.state.read() {
      return Ok(state.playback_rate);
    }

    Ok(1.0)
  }

  /// Sets the playback rate
  #[napi(setter)]
  #[allow(dead_code)]
  pub fn set_playback_rate(&mut self, playback_rate: f64) -> napi::Result<()> {
    if let Ok(mut state) = self.state.write() {
      state.playback_rate = playback_rate;
    }
    Ok(())
  }

  /// Gets the playback status
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_playback_status(&self) -> napi::Result<MediaPlayerPlaybackStatus> {
    if let Ok(state) = self.state.read() {
      return Ok(state.playback_status);
    }

    Ok(MediaPlayerPlaybackStatus::Unknown)
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

    let patch_result: PlaybackPatchResult = self.update_playback_state(PlaybackStatePatch {
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
    if let Ok(state) = self.state.read() {
      return Ok(state.media_type);
    }

    Ok(MediaPlayerMediaType::Unknown)
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

    if let Ok(mut state) = self.state.write() {
      state.media_type = media_type;
    }

    Ok(())
  }

  /// Gets the media title
  #[napi(getter)]
  #[allow(dead_code)]
  pub fn get_title(&self) -> napi::Result<String> {
    if let Ok(state) = self.state.read() {
      return Ok(state.title.to_owned());
    }

    Ok(String::new())
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
    if let Ok(state) = self.state.read() {
      return Ok(state.artist.to_owned());
    }

    Ok(String::new())
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
    if let Ok(state) = self.state.read() {
      return Ok(state.album_title.to_owned());
    }

    Ok(String::new())
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
    if let Ok(state) = self.state.read() {
      return Ok(state.track_id.to_owned());
    }

    Ok(String::new())
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

  #[allow(dead_code)]
  fn publish_state(&mut self) -> napi::Result<()> {
    self.flush_state(FlushMode::Full)
  }

  #[allow(dead_code)]
  fn update_metadata(&mut self) -> napi::Result<()> {
    self.flush_state(FlushMode::MetadataOnly)
  }

  #[allow(dead_code)]
  fn update_playback(&mut self) -> napi::Result<()> {
    self.flush_state(FlushMode::PlaybackOnly)
  }

  fn update_title_data(&mut self, patch: TitleDataPatch, flush_mode: FlushMode) -> napi::Result<()> {
    let mut did_change: bool = false;
    if let Ok(mut state) = self.state.write() {
      if let Some(title) = patch.title {
        if state.title != title {
          state.title = title;
          did_change = true;
        }
      }
      if let Some(artist) = patch.artist {
        if state.artist != artist {
          state.artist = artist;
          did_change = true;
        }
      }
      if let Some(album_title) = patch.album_title {
        if state.album_title != album_title {
          state.album_title = album_title;
          did_change = true;
        }
      }
      if let Some(thumbnail) = patch.thumbnail {
        if state.thumbnail != thumbnail {
          state.thumbnail = thumbnail;
          did_change = true;
        }
      }
      if let Some(track_id) = patch.track_id {
        if state.track_id != track_id {
          state.track_id = track_id;
          state.track_revision = state.track_revision.saturating_add(1);
          state.track_transition_pending = true;
          state.duration = 0.0;
          state.position = 0.0;
          state.playback_dirty = true;
          state.prefer_last_playback_position_for_status_flush = false;
          did_change = true;
        }
      }

      if did_change {
        state.state_revision = state.state_revision.saturating_add(1);
        state.metadata_dirty = true;
      }
    }

    if !did_change {
      return Ok(());
    }

    self.flush_state(flush_mode)
  }

  fn update_playback_state(&mut self, patch: PlaybackStatePatch) -> PlaybackPatchResult {
    if let Ok(mut state) = self.state.write() {
      let mut playback_changed: bool = false;
      let mut duration_changed: bool = false;
      let mut playback_status_changed: bool = false;
      let mut completed_track_transition: bool = false;

      if let Some(duration) = patch.duration {
        if (state.duration - duration).abs() > f64::EPSILON {
          state.duration = duration;
          duration_changed = true;
        }
      }

      if let Some(position) = patch.position {
        if (state.position - position).abs() > f64::EPSILON {
          state.position = position;
          playback_changed = true;
        }
      }

      if let Some(playback_status) = patch.playback_status {
        if state.playback_status != playback_status {
          state.playback_status = playback_status;
          playback_status_changed = true;
          playback_changed = true;
        }
      }

      if duration_changed {
        state.metadata_dirty = true;
        playback_changed = true;
      }

      if state.track_transition_pending && (duration_changed || patch.position.is_some()) {
        state.position_event_track_revision = state.track_revision;
        state.track_transition_pending = false;
        completed_track_transition = true;
      }

      if patch.position.is_some() || duration_changed {
        state.prefer_last_playback_position_for_status_flush = false;
      } else if playback_status_changed {
        state.prefer_last_playback_position_for_status_flush = true;
      }

      if playback_changed {
        state.state_revision = state.state_revision.saturating_add(1);
        state.playback_dirty = true;
      }

      return PlaybackPatchResult {
        changed: playback_changed,
        completed_track_transition,
      };
    }

    PlaybackPatchResult {
      changed: false,
      completed_track_transition: false,
    }
  }

  fn flush_state(&mut self, flush_mode: FlushMode) -> napi::Result<()> {
    let payload: Option<FlushPayload> = self.create_flush_payload(flush_mode);
    let Some(payload) = payload else {
      return Ok(());
    };

    if let Some(metadata_snapshot) = payload.metadata.clone() {
      self.send_metadata(&metadata_snapshot)?;
      self.mark_metadata_flushed(payload.state_revision, metadata_snapshot);
    }

    if let Some(playback_snapshot) = payload.playback.clone() {
      self.send_playback(&playback_snapshot)?;
      self.mark_playback_flushed(payload.state_revision, playback_snapshot);
    }

    Ok(())
  }

  fn create_flush_payload(&self, flush_mode: FlushMode) -> Option<FlushPayload> {
    if let Ok(state) = self.state.read() {
      if !state.active {
        return None;
      }

      let metadata_requested: bool = matches!(
        flush_mode,
        FlushMode::Full | FlushMode::TrackChange | FlushMode::MetadataOnly
      );
      let playback_requested: bool = matches!(
        flush_mode,
        FlushMode::Full | FlushMode::TrackChange | FlushMode::PlaybackOnly
      );

      let metadata_snapshot: MetadataSnapshot = MetadataSnapshot {
        title: state.title.clone(),
        album_title: state.album_title.clone(),
        artist: state.artist.clone(),
        thumbnail: state.thumbnail.clone(),
        duration: state.duration.max(0.0),
      };
      let playback_position: f64 = if state.prefer_last_playback_position_for_status_flush
        && !state.track_transition_pending
      {
        state
          .last_playback_snapshot
          .as_ref()
          .map_or_else(|| state.position.max(0.0), |snapshot| snapshot.position.max(0.0))
      } else {
        state.position.max(0.0)
      };
      let playback_snapshot: PlaybackSnapshot = PlaybackSnapshot {
        playback_status: state.playback_status,
        position: playback_position,
      };

      let should_emit_metadata: bool = metadata_requested
        && (state.metadata_dirty
          || state.last_metadata_snapshot.as_ref() != Some(&metadata_snapshot));
      let should_emit_playback: bool = playback_requested
        && (state.playback_dirty
          || state.last_playback_snapshot.as_ref() != Some(&playback_snapshot));

      if !should_emit_metadata && !should_emit_playback {
        return None;
      }

      return Some(FlushPayload {
        state_revision: state.state_revision,
        metadata: if should_emit_metadata {
          Some(metadata_snapshot)
        } else {
          None
        },
        playback: if should_emit_playback {
          Some(playback_snapshot)
        } else {
          None
        },
      });
    }

    None
  }

  fn send_metadata(&mut self, metadata_snapshot: &MetadataSnapshot) -> napi::Result<()> {
    let metadata: MediaMetadata<'_> = MediaMetadata {
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
    let progress: Option<MediaPosition> =
      Some(MediaPosition(Duration::from_secs_f64(playback_snapshot.position)));
    let playback: MediaPlayback = match playback_snapshot.playback_status {
      MediaPlayerPlaybackStatus::Playing => MediaPlayback::Playing { progress },
      MediaPlayerPlaybackStatus::Paused => MediaPlayback::Paused { progress },
      _ => MediaPlayback::Stopped,
    };

    self
      .media_controls
      .set_playback(playback)
      .map_err(map_souvlaki_error)
  }

  fn mark_metadata_flushed(&self, state_revision: u64, metadata_snapshot: MetadataSnapshot) {
    if let Ok(mut state) = self.state.write() {
      state.last_metadata_snapshot = Some(metadata_snapshot);
      if state.state_revision == state_revision {
        state.metadata_dirty = false;
      }
    }
  }

  fn mark_playback_flushed(&self, state_revision: u64, playback_snapshot: PlaybackSnapshot) {
    if let Ok(mut state) = self.state.write() {
      state.last_playback_snapshot = Some(playback_snapshot);
      if state.state_revision == state_revision {
        state.playback_dirty = false;
        state.prefer_last_playback_position_for_status_flush = false;
      }
    }
  }

  #[cfg(test)]
  fn test_apply_title_data_patch(
    state: &mut MediaPlayerState,
    patch: TitleDataPatch,
  ) -> bool {
    let mut did_change: bool = false;
    if let Some(title) = patch.title {
      if state.title != title {
        state.title = title;
        did_change = true;
      }
    }
    if let Some(artist) = patch.artist {
      if state.artist != artist {
        state.artist = artist;
        did_change = true;
      }
    }
    if let Some(album_title) = patch.album_title {
      if state.album_title != album_title {
        state.album_title = album_title;
        did_change = true;
      }
    }
    if let Some(thumbnail) = patch.thumbnail {
      if state.thumbnail != thumbnail {
        state.thumbnail = thumbnail;
        did_change = true;
      }
    }
    if let Some(track_id) = patch.track_id {
      if state.track_id != track_id {
        state.track_id = track_id;
        state.track_revision = state.track_revision.saturating_add(1);
        state.track_transition_pending = true;
        state.duration = 0.0;
        state.position = 0.0;
        state.playback_dirty = true;
        state.prefer_last_playback_position_for_status_flush = false;
        did_change = true;
      }
    }
    if did_change {
      state.metadata_dirty = true;
      state.state_revision = state.state_revision.saturating_add(1);
    }
    did_change
  }

  #[cfg(test)]
  fn test_apply_playback_state_patch(
    state: &mut MediaPlayerState,
    patch: PlaybackStatePatch,
  ) -> PlaybackPatchResult {
    let mut playback_changed: bool = false;
    let mut duration_changed: bool = false;
    let mut playback_status_changed: bool = false;
    let mut completed_track_transition: bool = false;

    if let Some(duration) = patch.duration {
      if (state.duration - duration).abs() > f64::EPSILON {
        state.duration = duration;
        duration_changed = true;
      }
    }
    if let Some(position) = patch.position {
      if (state.position - position).abs() > f64::EPSILON {
        state.position = position;
        playback_changed = true;
      }
    }
    if let Some(playback_status) = patch.playback_status {
      if state.playback_status != playback_status {
        state.playback_status = playback_status;
        playback_status_changed = true;
        playback_changed = true;
      }
    }
    if duration_changed {
      state.metadata_dirty = true;
      playback_changed = true;
    }
    if state.track_transition_pending && (duration_changed || patch.position.is_some()) {
      state.position_event_track_revision = state.track_revision;
      state.track_transition_pending = false;
      completed_track_transition = true;
    }
    if patch.position.is_some() || duration_changed {
      state.prefer_last_playback_position_for_status_flush = false;
    } else if playback_status_changed {
      state.prefer_last_playback_position_for_status_flush = true;
    }
    if playback_changed {
      state.playback_dirty = true;
      state.state_revision = state.state_revision.saturating_add(1);
    }
    PlaybackPatchResult {
      changed: playback_changed,
      completed_track_transition,
    }
  }

  #[cfg(test)]
  fn test_should_accept_set_position(state: &MediaPlayerState, requested_seconds: f64) -> bool {
    requested_seconds <= state.duration
      && state.can_seek
      && state.position_event_track_revision == state.track_revision
  }

  #[cfg(test)]
  fn test_should_emit_metadata(
    state: &MediaPlayerState,
    metadata_snapshot: &MetadataSnapshot,
    flush_mode: FlushMode,
  ) -> bool {
    let metadata_requested: bool = matches!(
      flush_mode,
      FlushMode::Full | FlushMode::TrackChange | FlushMode::MetadataOnly
    );
    metadata_requested
      && (state.metadata_dirty || state.last_metadata_snapshot.as_ref() != Some(metadata_snapshot))
  }

  #[cfg(test)]
  fn test_should_emit_playback(
    state: &MediaPlayerState,
    playback_snapshot: &PlaybackSnapshot,
    flush_mode: FlushMode,
  ) -> bool {
    let playback_requested: bool = matches!(
      flush_mode,
      FlushMode::Full | FlushMode::TrackChange | FlushMode::PlaybackOnly
    );
    playback_requested
      && (state.playback_dirty || state.last_playback_snapshot.as_ref() != Some(playback_snapshot))
  }

  #[cfg(test)]
  fn test_metadata_snapshot_from_state(state: &MediaPlayerState) -> MetadataSnapshot {
    MetadataSnapshot {
      title: state.title.clone(),
      album_title: state.album_title.clone(),
      artist: state.artist.clone(),
      thumbnail: state.thumbnail.clone(),
      duration: state.duration.max(0.0),
    }
  }

  #[cfg(test)]
  fn test_playback_snapshot_from_state(state: &MediaPlayerState) -> PlaybackSnapshot {
    let playback_position: f64 = if state.prefer_last_playback_position_for_status_flush
      && !state.track_transition_pending
    {
      state
        .last_playback_snapshot
        .as_ref()
        .map_or_else(|| state.position.max(0.0), |snapshot| snapshot.position.max(0.0))
    } else {
      state.position.max(0.0)
    };

    PlaybackSnapshot {
      playback_status: state.playback_status,
      position: playback_position,
    }
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
  button_pressed_listeners: &Arc<
    DashMap<usize, ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>,
  >,
  playback_position_changed_listeners: &Arc<
    DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>,
  >,
  playback_position_seeked_listeners: &Arc<
    DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>,
  >,
) {
  let Ok(current_state) = state.read() else {
    return;
  };

  if !current_state.active {
    return;
  }

  match event {
    MediaControlEvent::Play if current_state.can_play => {
      emit_button_pressed(button_pressed_listeners, "play");
    }
    MediaControlEvent::Pause if current_state.can_pause => {
      emit_button_pressed(button_pressed_listeners, "pause");
    }
    MediaControlEvent::Toggle if current_state.can_play || current_state.can_pause => {
      emit_button_pressed(button_pressed_listeners, "playpause");
    }
    MediaControlEvent::Next if current_state.can_go_next => {
      emit_button_pressed(button_pressed_listeners, "next");
    }
    MediaControlEvent::Previous if current_state.can_go_previous => {
      emit_button_pressed(button_pressed_listeners, "previous");
    }
    MediaControlEvent::Stop if current_state.can_control => {
      emit_button_pressed(button_pressed_listeners, "stop");
    }
    MediaControlEvent::SeekBy(direction, amount) if current_state.can_seek => {
      let signed_seconds: f64 = match direction {
        SeekDirection::Forward => amount.as_secs_f64(),
        SeekDirection::Backward => -amount.as_secs_f64(),
      };
      emit_seek(playback_position_seeked_listeners, signed_seconds);
    }
    MediaControlEvent::SetPosition(position) if current_state.can_seek => {
      let requested_seconds: f64 = position.0.as_secs_f64();
      let is_track_context_current: bool =
        current_state.position_event_track_revision == current_state.track_revision;
      if requested_seconds <= current_state.duration && is_track_context_current {
        emit_position(playback_position_changed_listeners, requested_seconds);
      }
    }
    _ => {}
  }
}

fn emit_button_pressed(
  listeners: &Arc<DashMap<usize, ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>>,
  button: &str,
) {
  for listener in listeners.iter() {
    listener.call(
      Ok(button.to_string()),
      ThreadsafeFunctionCallMode::NonBlocking,
    );
  }
}

fn emit_position(
  listeners: &Arc<DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>>,
  position: f64,
) {
  for listener in listeners.iter() {
    listener.call(Ok(position), ThreadsafeFunctionCallMode::NonBlocking);
  }
}

fn emit_seek(
  listeners: &Arc<DashMap<usize, ThreadsafeFunction<f64, ErrorStrategy::CalleeHandled>>>,
  seek_delta: f64,
) {
  for listener in listeners.iter() {
    listener.call(Ok(seek_delta), ThreadsafeFunctionCallMode::NonBlocking);
  }
}

fn to_optional_ref(value: &str) -> Option<&str> {
  if value.is_empty() {
    None
  } else {
    Some(value)
  }
}

#[cfg(test)]
mod tests {
  use super::{
    FlushMode, MediaPlayer, MediaPlayerPlaybackStatus, MediaPlayerState, MetadataSnapshot,
    PlaybackSnapshot, PlaybackStatePatch, TitleDataPatch,
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

  #[test]
  fn track_transition_is_completed_by_timeline_patch() {
    let mut state: MediaPlayerState = build_test_state();

    let title_changed: bool = MediaPlayer::test_apply_title_data_patch(
      &mut state,
      TitleDataPatch {
        track_id: Some(String::from("track-2")),
        ..TitleDataPatch::default()
      },
    );
    assert!(title_changed);
    assert!(state.track_transition_pending);
    assert_eq!(state.track_revision, 1);
    assert_eq!(state.position_event_track_revision, 0);

    let playback_result = MediaPlayer::test_apply_playback_state_patch(
      &mut state,
      PlaybackStatePatch {
        duration: Some(200.0),
        position: Some(0.0),
        ..PlaybackStatePatch::default()
      },
    );

    assert!(playback_result.completed_track_transition);
    assert_eq!(state.position_event_track_revision, state.track_revision);
    assert!(!state.track_transition_pending);
  }

  #[test]
  fn set_position_is_rejected_for_stale_track_context() {
    let mut state: MediaPlayerState = build_test_state();
    state.track_revision = 2;
    state.position_event_track_revision = 1;
    state.duration = 120.0;
    state.can_seek = true;

    assert!(!MediaPlayer::test_should_accept_set_position(&state, 12.0));

    state.position_event_track_revision = 2;
    assert!(MediaPlayer::test_should_accept_set_position(&state, 12.0));
  }

  #[test]
  fn metadata_flush_requires_dirty_or_changed_snapshot() {
    let mut state: MediaPlayerState = build_test_state();
    state.title = String::from("Song");
    state.duration = 180.0;

    let metadata_snapshot: MetadataSnapshot = MediaPlayer::test_metadata_snapshot_from_state(&state);
    state.last_metadata_snapshot = Some(metadata_snapshot.clone());
    state.metadata_dirty = false;

    assert!(!MediaPlayer::test_should_emit_metadata(
      &state,
      &metadata_snapshot,
      FlushMode::PlaybackOnly
    ));
    assert!(!MediaPlayer::test_should_emit_metadata(
      &state,
      &metadata_snapshot,
      FlushMode::TrackChange
    ));

    state.metadata_dirty = true;
    assert!(MediaPlayer::test_should_emit_metadata(
      &state,
      &metadata_snapshot,
      FlushMode::TrackChange
    ));
  }

  #[test]
  fn paused_playback_flush_uses_latest_position_snapshot() {
    let mut state: MediaPlayerState = build_test_state();
    state.position = 55.0;
    state.playback_status = MediaPlayerPlaybackStatus::Playing;
    state.last_playback_snapshot = Some(PlaybackSnapshot {
      playback_status: MediaPlayerPlaybackStatus::Playing,
      position: 58.0,
    });
    state.playback_dirty = false;

    let changed: bool = MediaPlayer::test_apply_playback_state_patch(
      &mut state,
      PlaybackStatePatch {
        playback_status: Some(MediaPlayerPlaybackStatus::Paused),
        ..PlaybackStatePatch::default()
      },
    )
    .changed;
    assert!(changed);
    assert!(state.prefer_last_playback_position_for_status_flush);

    let playback_snapshot: PlaybackSnapshot = MediaPlayer::test_playback_snapshot_from_state(&state);
    assert_eq!(playback_snapshot.position, 58.0);
    assert_eq!(playback_snapshot.playback_status, MediaPlayerPlaybackStatus::Paused);
    assert!(MediaPlayer::test_should_emit_playback(
      &state,
      &playback_snapshot,
      FlushMode::PlaybackOnly
    ));
  }

  #[test]
  fn track_change_resets_timeline_state_before_next_playback_flush() {
    let mut state: MediaPlayerState = build_test_state();
    state.duration = 245.0;
    state.position = 182.0;
    state.last_playback_snapshot = Some(PlaybackSnapshot {
      playback_status: MediaPlayerPlaybackStatus::Playing,
      position: 182.0,
    });

    let title_changed: bool = MediaPlayer::test_apply_title_data_patch(
      &mut state,
      TitleDataPatch {
        track_id: Some(String::from("new-track")),
        ..TitleDataPatch::default()
      },
    );

    assert!(title_changed);
    assert_eq!(state.duration, 0.0);
    assert_eq!(state.position, 0.0);
    assert!(state.track_transition_pending);
  }
}

fn map_souvlaki_error(error: souvlaki::Error) -> napi::Error {
  napi::Error::from_reason(format!("{:?}", error))
}


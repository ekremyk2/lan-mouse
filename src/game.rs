//! Detects when a game is in the foreground, so mouse sharing can pause while
//! playing: the screen edges stop handing the mouse to other devices, and
//! other devices can't move their mouse into the game.
//!
//! Games are discovered automatically. A program counts as a game if
//! - Windows recognized it as one (the Game Bar's `GameConfigStore`),
//! - it is installed in a game launcher's library (Steam, Epic, GOG, Ubisoft,
//!   Xbox, EA, Riot), found via the launchers' own install lists or folders,
//! - or it runs in exclusive fullscreen.
//!
//! The optional `game_programs` config option adds programs by name.
//! Only implemented on Windows so far.
#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::HashSet;

use tokio::{sync::watch, task::JoinHandle};

/// folders game launchers install games into (lowercase)
const GAME_LIBRARIES: &[&str] = &[
    r"\steamapps\common\",
    r"\epic games\",
    r"\xboxgames\",
    r"\gog galaxy\games\",
    r"\riot games\",
    r"\ea games\",
    r"\ubisoft game launcher\games\",
];

/// launchers living inside those folders (lowercase)
const NOT_GAMES: &[&str] = &[r"\epic games\launcher\"];

/// Watches the foreground program and publishes the game in front, if any.
pub(crate) struct GameWatch {
    rx: watch::Receiver<Option<String>>,
    task: Option<JoinHandle<()>>,
    /// keeps the channel open while no watcher task owns the sender
    _tx: Option<watch::Sender<Option<String>>>,
}

impl GameWatch {
    /// `programs`: additional executable names to treat as games
    pub(crate) fn new(enabled: bool, programs: Vec<String>) -> Self {
        let (tx, rx) = watch::channel(None);
        #[cfg(windows)]
        if enabled {
            log::info!("mouse sharing pauses while a game is in the foreground");
            let task = tokio::task::spawn_local(windows_impl::watch_foreground(tx, programs));
            return Self {
                rx,
                task: Some(task),
                _tx: None,
            };
        }
        #[cfg(not(windows))]
        let _ = (enabled, programs);
        Self {
            rx,
            task: None,
            _tx: Some(tx),
        }
    }

    pub(crate) fn subscribe(&self) -> GameState {
        GameState(self.rx.clone())
    }

    pub(crate) fn terminate(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Whether a game is in the foreground, as seen by [`GameWatch`].
#[derive(Clone)]
pub(crate) struct GameState(watch::Receiver<Option<String>>);

impl GameState {
    pub(crate) fn active(&self) -> bool {
        self.0.borrow().is_some()
    }

    /// wait until a game enters or leaves the foreground
    ///
    /// Cancel safe. Never returns once the watcher is gone.
    pub(crate) async fn changed(&mut self) {
        if self.0.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// games installed on this device, as reported by Windows and game launchers
#[derive(Debug, Default)]
struct InstalledGames {
    /// lowercase paths of game executables
    programs: HashSet<String>,
    /// lowercase install folders of games, ending in `\`
    folders: Vec<String>,
}

impl InstalledGames {
    fn add_program(&mut self, path: &str) {
        self.programs.insert(path.trim().to_lowercase());
    }

    fn add_folder(&mut self, folder: &str) {
        let mut folder = folder.trim().to_lowercase().replace('/', r"\");
        if folder.len() < 4 {
            // empty or a whole drive, e.g. `d:\`
            return;
        }
        if !folder.ends_with('\\') {
            folder.push('\\');
        }
        self.folders.push(folder);
    }

    /// whether the program at `path` is a game; `extra_programs` are
    /// lowercase executable names to treat as games as well
    fn contains(&self, path: &str, extra_programs: &[String]) -> bool {
        let path = path.to_lowercase();
        let name = path.rsplit(['\\', '/']).next().unwrap_or(&path);
        if NOT_GAMES.iter().any(|dir| path.contains(dir)) {
            return false;
        }
        self.programs.contains(&path)
            || extra_programs.iter().any(|program| program == name)
            || self.folders.iter().any(|folder| path.starts_with(folder))
            || GAME_LIBRARIES.iter().any(|dir| path.contains(dir))
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::time::{Duration, Instant};

    use tokio::sync::watch;
    use windows::{
        Win32::{
            Foundation::CloseHandle,
            System::{
                Registry::{
                    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ,
                    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
                },
                Threading::{
                    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
                    QueryFullProcessImageNameW,
                },
            },
            UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
        },
        core::{PCWSTR, PWSTR, w},
    };

    use super::InstalledGames;

    /// how often the foreground program is checked
    const CHECK_INTERVAL: Duration = Duration::from_secs(1);
    /// how often the installed games are looked up again
    const INSTALLED_GAMES_REFRESH: Duration = Duration::from_secs(60);

    pub(super) async fn watch_foreground(tx: watch::Sender<Option<String>>, programs: Vec<String>) {
        let programs = programs
            .iter()
            .map(|p| p.to_lowercase())
            .collect::<Vec<_>>();
        let mut installed = InstalledGames::default();
        let mut installed_read: Option<Instant> = None;
        let mut interval = tokio::time::interval(CHECK_INTERVAL);
        loop {
            interval.tick().await;
            if installed_read.is_none_or(|read| read.elapsed() > INSTALLED_GAMES_REFRESH) {
                installed = installed_games();
                installed_read = Some(Instant::now());
            }
            let game = foreground_game(&installed, &programs);
            tx.send_if_modified(|current| {
                if *current == game {
                    return false;
                }
                match &game {
                    Some(name) => log::info!("{name} is in the foreground: pausing mouse sharing"),
                    None => log::info!("no game in the foreground: resuming mouse sharing"),
                }
                current.clone_from(&game);
                true
            });
        }
    }

    /// file name of the foreground program, if it is a game
    fn foreground_game(installed: &InstalledGames, programs: &[String]) -> Option<String> {
        let path = foreground_program()?;
        if installed.contains(&path, programs) || exclusive_fullscreen() {
            let name = path.rsplit('\\').next().unwrap_or(&path);
            Some(name.to_owned())
        } else {
            None
        }
    }

    /// full path of the program owning the foreground window
    fn foreground_program() -> Option<String> {
        unsafe {
            let window = GetForegroundWindow();
            if window.is_invalid() {
                return None;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(window, Some(&mut pid));
            if pid == 0 {
                return None;
            }
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut path = [0u16; 1024];
            let mut len = path.len() as u32;
            let result = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut len,
            );
            let _ = CloseHandle(process);
            result.ok()?;
            Some(String::from_utf16_lossy(&path[..len as usize]))
        }
    }

    /// whether a Direct3D program runs in exclusive fullscreen
    fn exclusive_fullscreen() -> bool {
        // declared here to avoid compiling the huge `Win32_UI_Shell` bindings
        #[link(name = "shell32")]
        unsafe extern "system" {
            fn SHQueryUserNotificationState(state: *mut i32) -> i32;
        }
        const QUNS_RUNNING_D3D_FULL_SCREEN: i32 = 3;
        let mut state = 0;
        let queried = unsafe { SHQueryUserNotificationState(&mut state) } == 0;
        queried && state == QUNS_RUNNING_D3D_FULL_SCREEN
    }

    /// games Windows recognized and games installed by launchers
    fn installed_games() -> InstalledGames {
        let mut games = InstalledGames::default();
        // games Windows (Game Bar) recognized, wherever they are installed
        for path in registry_values(
            HKEY_CURRENT_USER,
            w!(r"System\GameConfigStore\Children"),
            w!("MatchedExeFullPath"),
        ) {
            games.add_program(&path);
        }
        for folder in registry_values(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\WOW6432Node\GOG.com\Games"),
            w!("path"),
        ) {
            games.add_folder(&folder);
        }
        for folder in registry_values(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\WOW6432Node\Ubisoft\Launcher\Installs"),
            w!("InstallDir"),
        ) {
            games.add_folder(&folder);
        }
        for folder in epic_install_folders() {
            games.add_folder(&folder);
        }
        log::debug!(
            "{} game programs and {} game folders installed",
            games.programs.len(),
            games.folders.len()
        );
        games
    }

    /// install folders from the Epic Games launcher's manifests
    fn epic_install_folders() -> Vec<String> {
        let Ok(program_data) = std::env::var("ProgramData") else {
            return vec![];
        };
        let manifests =
            std::path::Path::new(&program_data).join(r"Epic\EpicGamesLauncher\Data\Manifests");
        let Ok(entries) = std::fs::read_dir(manifests) else {
            return vec![];
        };
        entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "item"))
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .filter_map(|manifest| serde_json::from_str::<serde_json::Value>(&manifest).ok())
            .filter_map(|manifest| manifest["InstallLocation"].as_str().map(str::to_owned))
            .collect()
    }

    /// the string value `value` of every direct subkey of `root\path`
    fn registry_values(root: HKEY, path: PCWSTR, value: PCWSTR) -> Vec<String> {
        let mut values = vec![];
        unsafe {
            let mut key = HKEY::default();
            if RegOpenKeyExW(root, path, None, KEY_READ, &mut key).is_err() {
                return values;
            }
            for index in 0.. {
                let mut name = [0u16; 256];
                let mut name_len = name.len() as u32;
                if RegEnumKeyExW(
                    key,
                    index,
                    Some(PWSTR(name.as_mut_ptr())),
                    &mut name_len,
                    None,
                    None,
                    None,
                    None,
                )
                .is_err()
                {
                    break;
                }
                let mut data = [0u16; 1024];
                let mut size = (data.len() * 2) as u32;
                if RegGetValueW(
                    key,
                    PCWSTR(name.as_ptr()),
                    value,
                    RRF_RT_REG_SZ,
                    None,
                    Some(data.as_mut_ptr().cast()),
                    Some(&mut size),
                )
                .is_ok()
                {
                    // `size` is in bytes and includes the terminating null
                    let len = (size as usize / 2).saturating_sub(1);
                    values.push(String::from_utf16_lossy(&data[..len]));
                }
            }
            let _ = RegCloseKey(key);
        }
        values
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_games() {
        let mut installed = InstalledGames::default();
        installed.add_program(r"D:\Games\Half Sword\HalfSword.exe");
        installed.add_folder(r"D:\Games\Hollow Knight");
        installed.add_folder(r"D:\");
        let extra = vec!["mygame.exe".to_owned()];
        let game = |path: &str| installed.contains(path, &extra);

        // known to Windows / installed by a launcher
        assert!(game(r"D:\Games\Half Sword\HalfSword.exe"));
        assert!(game(r"d:\games\hollow knight\hollow_knight.exe"));
        // in a launcher's library folder
        assert!(game(
            r"C:\Program Files (x86)\Steam\steamapps\common\PEAK\PEAK.exe"
        ));
        assert!(game(
            r"E:\SteamLibrary\steamapps\common\Teardown\teardown.exe"
        ));
        assert!(game(
            r"C:\Program Files\Epic Games\Fortnite\FortniteClient.exe"
        ));
        // listed by name
        assert!(game(r"C:\Users\Sam\source\repos\MyGame\Build\MyGame.exe"));

        assert!(!game(r"C:\Program Files (x86)\Steam\steam.exe"));
        assert!(!game(
            r"C:\Program Files (x86)\Epic Games\Launcher\Portal\EpicGamesLauncher.exe"
        ));
        assert!(!game(
            r"C:\Program Files\Google\Chrome\Application\chrome.exe"
        ));
        // a whole drive is never treated as a game folder
        assert!(!game(r"D:\Tools\editor.exe"));
        // a folder only matches as a whole path component
        assert!(!game(r"D:\Games\Hollow Knight Notes\notes.exe"));
    }
}

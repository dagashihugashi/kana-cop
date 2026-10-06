use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::thread::{self, Thread};
use std::time::Duration;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};

// Windows API のインポート
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::Ime::ImmGetDefaultIMEWnd;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_OEM_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetGUIThreadInfo, GetMessageW,
    GetWindowThreadProcessId, SendMessageTimeoutW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, GUITHREADINFO, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG, SMTO_ABORTIFHUNG,
    WH_KEYBOARD_LL, WM_IME_CONTROL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

// 初期状態はオン（true）にしておく
static IS_ACTIVE: AtomicBool = AtomicBool::new(true);
static HOOK_HANDLE: OnceLock<HHOOK> = OnceLock::new();

// フックからIMEオフ担当スレッドへの依頼。フック内では重い処理をせず、これを立てて起こすだけにする
static IME_OFF_REQUESTED: AtomicBool = AtomicBool::new(false);
static IME_OFF_WORKER: OnceLock<Thread> = OnceLock::new();

const IMC_GETCONVERSIONMODE: usize = 1;
const IMC_SETCONVERSIONMODE: usize = 2;
const IMC_SETOPENSTATUS: usize = 6;

// IME変換モードのビット定義（imm.h の IME_CMODE_*）
const IME_CMODE_NATIVE: u32 = 0x1; // 日本語モード(ひらがな/カタカナ)か
const IME_CMODE_KATAKANA: u32 = 0x2; // カタカナか（NATIVEと併用して判定）
const IME_CMODE_FULLSHAPE: u32 = 0x8; // 全角か

// 相手ウィンドウがこれ以上応答しなければ、そのティックは諦めて次に回す
const IME_MESSAGE_TIMEOUT_MS: u32 = 100;
const PATROL_INTERVAL: Duration = Duration::from_millis(30);

// --- 1. Windows APIによる「入力警察」のコア機能 ---

// 現在IMEを制御すべきウィンドウを解決する。前面ウィンドウが無い、または自プロセスのもの
// (トレイメニュー表示中など)なら何もしないようNoneを返す。
//
// Tauri(WebView2)やTeamsのように、トップレベルの外枠ウィンドウと実際にキー入力を
// 受けている内側のウィンドウが別物になっているケースがあるため、GetGUIThreadInfoで
// 前面スレッドのフォーカスウィンドウを取得してそちらを優先する。
// 以前使っていたAttachThreadInputと違い相手の入力キューに相乗りしないので、相手が応答なしでも
// 巻き込まれない。呼び出しも軽いのでキャッシュせず毎回解決し、同一ウィンドウ内のフォーカス移動にも追従する
unsafe fn resolve_ime_wnd() -> Option<HWND> {
    let fg = GetForegroundWindow();
    if fg.0 == 0 {
        return None;
    }

    let mut pid = 0u32;
    let fg_thread = GetWindowThreadProcessId(fg, Some(&mut pid as *mut u32));
    if pid == std::process::id() {
        return None;
    }

    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    let has_focus = GetGUIThreadInfo(fg_thread, &mut info).is_ok() && info.hwndFocus.0 != 0;
    let target = if has_focus { info.hwndFocus } else { fg };

    let hime = ImmGetDefaultIMEWnd(target);
    (hime.0 != 0).then_some(hime)
}

// WM_IME_CONTROLを送る。応答なしの相手(SMTO_ABORTIFHUNG)や処理が遅い相手は待たずに諦め、Noneを返す
unsafe fn send_ime_control(hime: HWND, command: usize, value: isize) -> Option<usize> {
    let mut result = 0usize;
    let sent = SendMessageTimeoutW(
        hime,
        WM_IME_CONTROL,
        WPARAM(command),
        LPARAM(value),
        SMTO_ABORTIFHUNG,
        IME_MESSAGE_TIMEOUT_MS,
        Some(&mut result as *mut usize),
    );
    (sent.0 != 0).then_some(result)
}

// 現在の変換モードから矯正後のモードを決める。矯正不要ならNone
fn corrected_mode(current_mode: u32) -> Option<u32> {
    let is_native = (current_mode & IME_CMODE_NATIVE) != 0;
    let is_katakana = (current_mode & IME_CMODE_KATAKANA) != 0;
    let is_fullshape = (current_mode & IME_CMODE_FULLSHAPE) != 0;

    if !is_native && is_fullshape {
        // 「日本語入力ではなく、かつ全角（＝全角アルファベット）」→ 半角英数へ
        Some(current_mode & !IME_CMODE_FULLSHAPE)
    } else if is_native && is_katakana {
        // 「日本語入力で、かつカタカナ（全角・半角とも）」→ ひらがなへ強制
        Some((current_mode & !IME_CMODE_KATAKANA) | IME_CMODE_FULLSHAPE)
    } else {
        None
    }
}

fn patrol_ime_mode() {
    unsafe {
        let Some(hime) = resolve_ime_wnd() else {
            return;
        };
        let Some(current_mode) = send_ime_control(hime, IMC_GETCONVERSIONMODE, 0) else {
            return;
        };
        if let Some(new_mode) = corrected_mode(current_mode as u32) {
            let _ = send_ime_control(hime, IMC_SETCONVERSIONMODE, new_mode as isize);
        }
    }
}

fn force_ime_off() {
    unsafe {
        if let Some(hime) = resolve_ime_wnd() {
            // IMC_SETOPENSTATUS に 0 (OFF) を送信し、確実に半角英数(A)にする
            let _ = send_ime_control(hime, IMC_SETOPENSTATUS, 0);
        }
    }
}

// --- 2. 警察の任務（バックグラウンド監視＆検問） ---

// フックから呼ぶ。IMEオフ担当スレッドを起こすだけで、すぐ戻る
fn request_ime_off() {
    IME_OFF_REQUESTED.store(true, Ordering::Release);
    if let Some(worker) = IME_OFF_WORKER.get() {
        worker.unpark();
    }
}

unsafe extern "system" fn low_level_keyboard_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code == HC_ACTION as i32 {
        let msg = w_param.0 as u32;
        if (msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN) && IS_ACTIVE.load(Ordering::Relaxed) {
            let kb = *(l_param.0 as *const KBDLLHOOKSTRUCT);
            let vk_code = kb.vkCode;

            let is_esc = vk_code == VK_ESCAPE.0 as u32;
            let is_ctrl_bracket = vk_code == VK_OEM_4.0 as u32
                && (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;

            if is_esc || is_ctrl_bracket {
                // フック内で相手ウィンドウに問い合わせると、相手が応答なしの時にフックごと止まる。
                // 実処理は別スレッドに任せ、ここでは依頼だけしてすぐチェーンに戻す
                request_ime_off();
            }
        }
    }
    // キーは奪わない: 必ずチェーンを継続する
    CallNextHookEx(None, n_code, w_param, l_param)
}

fn start_police_tasks() {
    // 任務A: パトロール（全角アルファベット撲滅 & カタカナ入力の取り締まり）
    thread::spawn(|| loop {
        if IS_ACTIVE.load(Ordering::Relaxed) {
            patrol_ime_mode();
        }
        thread::sleep(PATROL_INTERVAL);
    });

    // 任務B: 出動（フックからの依頼を受けてIMEをオフにする）。フックより先に起動しておく
    let worker = thread::spawn(|| loop {
        thread::park();
        if IME_OFF_REQUESTED.swap(false, Ordering::AcqRel) {
            force_ime_off();
        }
    });
    let _ = IME_OFF_WORKER.set(worker.thread().clone());

    // 任務C: 検問（ESC / Ctrl+[ のグローバル低レベルフック監視）
    thread::spawn(|| unsafe {
        let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), None, 0) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("キーボードフックの設置に失敗しました: {e:?}");
                return;
            }
        };
        let _ = HOOK_HANDLE.set(hook);

        // メッセージポンプ（WH_KEYBOARD_LLに必須）
        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, HWND(0), 0, 0).0;
            if ret <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnhookWindowsHookEx(hook);
    });
}

// --- 3. Tauri本体の起動処理 (v2対応版) ---

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // アプリ起動と同時にバックグラウンド任務を開始！
    start_police_tasks();

    tauri::Builder::default()
        .setup(|app| {
            // メニューアイテムを作成
            let toggle_i = MenuItem::with_id(app, "toggle", "Pause", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Exit", true, None::<&str>)?;

            // メニューにセット
            let menu = Menu::with_items(app, &[&toggle_i, &quit_i])?;

            // メニューの文字を書き換えるためにクローンを持っておく
            let toggle_i_clone = toggle_i.clone();

            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .on_menu_event(move |_app, event| match event.id.as_ref() {
                    "toggle" => {
                        // 状態を反転
                        let current = IS_ACTIVE.load(Ordering::Relaxed);
                        IS_ACTIVE.store(!current, Ordering::Relaxed);

                        // v2の書き方でメニューのテキストを動的に変更
                        if current {
                            let _ = toggle_i_clone.set_text("Restart");
                        } else {
                            let _ = toggle_i_clone.set_text("Pause");
                        }
                    }
                    "quit" => {
                        std::process::exit(0);
                    }
                    _ => {}
                })
                .build(app)?;

            Ok(())
        })
        .plugin(tauri_plugin_opener::init())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    const IME_CMODE_ROMAN: u32 = 0x10;

    #[test]
    fn fullwidth_alphanumeric_is_corrected_to_halfwidth() {
        assert_eq!(corrected_mode(IME_CMODE_FULLSHAPE), Some(0));
    }

    #[test]
    fn fullwidth_katakana_is_corrected_to_hiragana() {
        let katakana = IME_CMODE_NATIVE | IME_CMODE_KATAKANA | IME_CMODE_FULLSHAPE;
        assert_eq!(
            corrected_mode(katakana),
            Some(IME_CMODE_NATIVE | IME_CMODE_FULLSHAPE)
        );
    }

    #[test]
    fn halfwidth_katakana_is_corrected_to_fullwidth_hiragana() {
        let halfwidth_katakana = IME_CMODE_NATIVE | IME_CMODE_KATAKANA;
        assert_eq!(
            corrected_mode(halfwidth_katakana),
            Some(IME_CMODE_NATIVE | IME_CMODE_FULLSHAPE)
        );
    }

    #[test]
    fn hiragana_is_left_alone() {
        assert_eq!(corrected_mode(IME_CMODE_NATIVE | IME_CMODE_FULLSHAPE), None);
    }

    #[test]
    fn halfwidth_alphanumeric_is_left_alone() {
        assert_eq!(corrected_mode(0), None);
    }

    #[test]
    fn unrelated_mode_bits_are_preserved() {
        assert_eq!(
            corrected_mode(IME_CMODE_ROMAN | IME_CMODE_FULLSHAPE),
            Some(IME_CMODE_ROMAN)
        );
        assert_eq!(
            corrected_mode(IME_CMODE_ROMAN | IME_CMODE_NATIVE | IME_CMODE_KATAKANA),
            Some(IME_CMODE_ROMAN | IME_CMODE_NATIVE | IME_CMODE_FULLSHAPE)
        );
    }
}

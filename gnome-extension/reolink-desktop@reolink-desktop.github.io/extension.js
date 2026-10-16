// Reolink Desktop — GNOME Shell extension.
//
// Adds a top-bar icon whose menu opens in the corner like the system menus,
// with a live-ish preview of the chosen cameras and shortcuts into the app.
// All camera work happens in the `reolink-desktop` app; this extension only talks
// to it over D-Bus (GApplication actions on io.github.ReolinkDesktop):
//   * state:   `cameras` (JSON) and `paused` action states
//   * preview: `preview-keepalive` makes the app write
//              $XDG_RUNTIME_DIR/reolink-desktop/preview/<id>.jpg a few times a second
//   * actions: show-live, show-camera(s), show-recordings, show-settings, toggle-pause, quit
//
// Left click opens the preview menu; right click opens a classic context menu
// (Live View, Recordings, Settings, Pause, Quit) like the StatusNotifier tray.

import Clutter from 'gi://Clutter';
import Cogl from 'gi://Cogl';
import GdkPixbuf from 'gi://GdkPixbuf';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Shell from 'gi://Shell';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const APP_ID = 'io.github.ReolinkDesktop';
const APP_PATH = '/io/github/ReolinkDesktop';
const DESKTOP_ID = 'reolink-desktop.desktop';
// Owned while enabled so the app hides its own StatusNotifier icon.
const OWN_NAME = 'io.github.ReolinkDesktop.ShellExtension';

const MENU_WIDTH = 360;
const GAP = 8;
const REFRESH_MS = 300;
const KEEPALIVE_SECONDS = 2;

const ReolinkIndicator = GObject.registerClass(
class ReolinkIndicator extends PanelMenu.Button {
    _init() {
        super._init(0.5, 'Reolink');

        this._cameras = [];
        this._running = false;
        this._actions = null;
        this._actionSignals = [];
        this._tiles = new Map();
        this._focusWatch = null;

        // ── Panel button: camera icon + state dot ────────────────
        const box = new St.BoxLayout({style_class: 'panel-status-menu-box'});
        this._icon = new St.Icon({
            gicon: Gio.ThemedIcon.new('camera-web-symbolic'),
            style_class: 'system-status-icon',
        });
        this._dot = new St.Widget({
            style_class: 'reolink-panel-dot',
            y_align: Clutter.ActorAlign.CENTER,
            visible: false,
        });
        box.add_child(this._icon);
        box.add_child(this._dot);
        this.add_child(box);

        // ── Menu ─────────────────────────────────────────────────
        this.menu.box.add_style_class_name('reolink-menu');

        const previewItem = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        const previewBox = new St.BoxLayout({
            orientation: Clutter.Orientation.VERTICAL,
            x_expand: true,
            style_class: 'reolink-preview',
        });
        this._message = new St.Label({style_class: 'reolink-message', x_align: Clutter.ActorAlign.CENTER});
        this._grid = new St.BoxLayout({orientation: Clutter.Orientation.VERTICAL, style_class: 'reolink-grid'});
        previewBox.add_child(this._message);
        previewBox.add_child(this._grid);
        previewItem.add_child(previewBox);
        this.menu.addMenuItem(previewItem);

        const buttonsItem = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        this._buttons = new St.BoxLayout({style_class: 'reolink-buttons', x_expand: true});
        this._buttons.add_child(this._button('view-fullscreen-symbolic', 'Open', () => this._open('show-live')));
        this._buttons.add_child(this._button('document-open-recent-symbolic', 'Recordings', () => this._open('show-recordings')));
        this._buttons.add_child(this._button('emblem-system-symbolic', 'Settings', () => this._open('show-settings')));
        buttonsItem.add_child(this._buttons);
        this.menu.addMenuItem(buttonsItem);

        this._separator = new PopupMenu.PopupSeparatorMenuItem();
        this.menu.addMenuItem(this._separator);
        this._pauseItem = new PopupMenu.PopupSwitchMenuItem('Pause notifications', false);
        this._pauseItem.connect('toggled', () => this._activate('toggle-pause'));
        this.menu.addMenuItem(this._pauseItem);
        this._quitItem = new PopupMenu.PopupImageMenuItem('Quit', 'application-exit-symbolic');
        this._quitItem.connect('activate', () => this._activate('quit'));
        this.menu.addMenuItem(this._quitItem);

        this._buildContextMenu();

        this.menu.connect('open-state-changed', (_menu, open) => {
            if (open)
                this._startPreview();
            else
                this._stopPreview();
        });

        this._ownId = Gio.bus_own_name(Gio.BusType.SESSION, OWN_NAME,
            Gio.BusNameOwnerFlags.NONE, null, null, null);
        this._watchId = Gio.bus_watch_name(Gio.BusType.SESSION, APP_ID,
            Gio.BusNameWatcherFlags.NONE,
            connection => this._appAppeared(connection),
            () => this._appVanished());

        this.connect('destroy', () => this._onDestroy());
        this._sync();
    }

    _buildContextMenu() {
        this._contextMenu = new PopupMenu.PopupMenu(this, 0.5, St.Side.TOP);
        this._contextMenu.actor.add_style_class_name('panel-menu');
        Main.uiGroup.add_child(this._contextMenu.actor);
        this._contextMenu.actor.hide();
        // Own manager: in the panel's one, two menus on the same button make
        // hovering it switch from the preview menu to this one.
        this._contextMenuManager = new PopupMenu.PopupMenuManager(this);
        this._contextMenuManager.addMenu(this._contextMenu);

        const add = (label, iconName, callback) => {
            const item = new PopupMenu.PopupImageMenuItem(label, iconName);
            item.connect('activate', callback);
            this._contextMenu.addMenuItem(item);
            return item;
        };
        this._ctxRunning = [
            add('Live View', 'camera-web-symbolic', () => this._open('show-live')),
            add('Recordings', 'document-open-recent-symbolic', () => this._open('show-recordings')),
            add('Settings…', 'emblem-system-symbolic', () => this._open('show-settings')),
        ];
        this._ctxPauseSeparator = new PopupMenu.PopupSeparatorMenuItem();
        this._contextMenu.addMenuItem(this._ctxPauseSeparator);
        this._ctxPauseItem = new PopupMenu.PopupSwitchMenuItem('Pause notifications', false);
        this._ctxPauseItem.connect('toggled', () => this._activate('toggle-pause'));
        this._contextMenu.addMenuItem(this._ctxPauseItem);
        this._ctxQuitSeparator = new PopupMenu.PopupSeparatorMenuItem();
        this._contextMenu.addMenuItem(this._ctxQuitSeparator);
        this._ctxRunning.push(
            add('Quit', 'application-exit-symbolic', () => this._activate('quit')));
    }

    /// Right click toggles the context menu; anything else goes to the
    /// preview menu (PanelMenu.Button's default behaviour).
    vfunc_event(event) {
        const type = event.type();
        if (type !== Clutter.EventType.BUTTON_PRESS && type !== Clutter.EventType.TOUCH_BEGIN)
            return super.vfunc_event(event);

        if (type === Clutter.EventType.BUTTON_PRESS &&
            event.get_button() === Clutter.BUTTON_SECONDARY) {
            this.menu.close();
            this._contextMenu.toggle();
            return Clutter.EVENT_STOP;
        }
        if (this._contextMenu.isOpen) {
            this._contextMenu.close();
            return Clutter.EVENT_STOP;
        }
        return super.vfunc_event(event);
    }

    // ── App connection ──────────────────────────────────────────

    _appAppeared(connection) {
        this._actions = Gio.DBusActionGroup.get(connection, APP_ID, APP_PATH);
        this._actionSignals = [
            this._actions.connect('action-state-changed', (_g, name, value) => this._onState(name, value)),
            this._actions.connect('action-added', (group, name) => this._onState(name, group.get_action_state(name))),
        ];
        // Triggers the initial fetch; states then arrive via action-added.
        this._actions.list_actions();
        for (const name of ['cameras', 'paused'])
            this._onState(name, this._actions.get_action_state(name));
        this._running = true;
        this._sync();
    }

    _appVanished() {
        this._disconnectActions();
        this._running = false;
        this._cameras = [];
        this._sync();
    }

    _disconnectActions() {
        if (this._actions) {
            for (const id of this._actionSignals)
                this._actions.disconnect(id);
        }
        this._actionSignals = [];
        this._actions = null;
    }

    _onState(name, value) {
        if (!value)
            return;
        if (name === 'cameras') {
            try {
                this._cameras = JSON.parse(value.deepUnpack());
            } catch (e) {
                logError(e, 'Reolink Desktop: invalid camera state');
                this._cameras = [];
            }
            this._sync();
        } else if (name === 'paused') {
            this._pauseItem.setToggleState(value.deepUnpack());
            this._ctxPauseItem.setToggleState(value.deepUnpack());
        }
    }

    _activate(name, parameter = null) {
        if (this._actions)
            this._actions.activate_action(name, parameter);
    }

    /// Runs an action that shows the app window, then gives it focus.
    _open(name, parameter = null) {
        this.menu.close();
        this._contextMenu.close();
        if (!this._running) {
            this._launchApp();
            return;
        }
        this._focusAppWindowWhenShown();
        this._activate(name, parameter);
    }

    _launchApp() {
        this.menu.close();
        this._contextMenu.close();
        const app = Shell.AppSystem.get_default().lookup_app(DESKTOP_ID);
        if (app)
            app.activate();
        else
            Main.notify('Reolink Desktop', 'The reolink-desktop app is not installed.');
    }

    /// Wayland focus-stealing prevention may leave a window opened over D-Bus
    /// in the background; as the compositor we can simply activate it.
    _focusAppWindowWhenShown() {
        const existing = global.get_window_actors()
            .map(a => a.meta_window)
            .find(w => w.get_gtk_application_id() === APP_ID && !w.minimized && w.showing_on_its_workspace());
        if (existing) {
            Main.activateWindow(existing);
            return;
        }
        this._cancelFocusWatch();
        const signalId = global.display.connect('window-created', (_d, window) => {
            if (window.get_gtk_application_id() !== APP_ID)
                return;
            this._cancelFocusWatch();
            const shownId = window.connect('shown', () => {
                window.disconnect(shownId);
                Main.activateWindow(window);
            });
        });
        // Give up after a few seconds.
        const timeoutId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, 5, () => {
            this._focusWatch.timeoutId = 0;
            this._cancelFocusWatch();
            return GLib.SOURCE_REMOVE;
        });
        this._focusWatch = {signalId, timeoutId};
    }

    _cancelFocusWatch() {
        if (!this._focusWatch)
            return;
        global.display.disconnect(this._focusWatch.signalId);
        if (this._focusWatch.timeoutId)
            GLib.source_remove(this._focusWatch.timeoutId);
        this._focusWatch = null;
    }

    // ── Menu content ────────────────────────────────────────────

    _sync() {
        const previewCams = this._cameras.filter(c => c.preview);

        let message = '';
        if (!this._running)
            message = 'Reolink Desktop is not running';
        else if (this._cameras.length === 0)
            message = 'No cameras configured';
        else if (previewCams.length === 0)
            message = 'No cameras selected for preview';
        this._message.text = message;
        this._message.visible = message !== '';

        this._buttons.visible = this._running;
        this._pauseItem.visible = this._running && this._cameras.length > 0;
        this._separator.visible = true;
        this._quitItem.visible = this._running;

        for (const item of this._ctxRunning)
            item.visible = this._running;
        this._ctxPauseSeparator.visible = this._pauseItem.visible;
        this._ctxPauseItem.visible = this._pauseItem.visible;
        this._ctxQuitSeparator.visible = this._running;

        this._rebuildTiles(previewCams);
        this._updatePanel();
    }

    _updatePanel() {
        // Like a tray icon, the button only exists while the app runs.
        this.visible = this._running;
        this.container.visible = this._running;
        if (!this._running) {
            this.menu.close();
            this._contextMenu.close();
        }
        const states = this._cameras.map(c => c.state);
        let dot = null;
        if (states.includes('alert'))
            dot = 'alert';
        else if (states.includes('offline'))
            dot = 'offline';
        this._dot.visible = dot !== null;
        this._dot.style_class = `reolink-panel-dot ${dot ?? ''}`;
    }

    _rebuildTiles(cameras) {
        const key = cameras.map(c => `${c.id}:${c.name}`).join('|');
        if (key !== this._tilesKey) {
            this._tilesKey = key;
            this._grid.destroy_all_children();
            this._tiles.clear();

            const columns = cameras.length > 1 ? 2 : 1;
            const width = Math.floor((MENU_WIDTH - GAP * (columns - 1)) / columns);
            const height = Math.round(width * 9 / 16);
            let row = null;
            cameras.forEach((cam, index) => {
                if (index % columns === 0) {
                    row = new St.BoxLayout({style_class: 'reolink-row'});
                    this._grid.add_child(row);
                }
                const tile = this._makeTile(cam, width, height);
                row.add_child(tile.button);
                this._tiles.set(cam.id, tile);
            });
        }
        for (const cam of cameras) {
            const tile = this._tiles.get(cam.id);
            if (!tile)
                continue;
            tile.chip.text = cam.name;
            tile.dot.style_class = `reolink-dot ${cam.state}`;
            tile.status.text = cam.state === 'offline' ? 'Offline' : 'Connecting…';
            tile.status.visible = !tile.hasFrame || cam.state === 'offline';
        }
    }

    _makeTile(cam, width, height) {
        const image = new St.Widget({x_expand: true, y_expand: true});
        image.set_content_gravity(Clutter.ContentGravity.RESIZE_ASPECT);

        const dot = new St.Widget({style_class: `reolink-dot ${cam.state}`, y_align: Clutter.ActorAlign.CENTER});
        const chip = new St.Label({text: cam.name, y_align: Clutter.ActorAlign.CENTER});
        // BinLayout only honours the alignment of expanding children.
        const chipBox = new St.BoxLayout({
            style_class: 'reolink-chip',
            x_expand: true,
            y_expand: true,
            x_align: Clutter.ActorAlign.START,
            y_align: Clutter.ActorAlign.START,
        });
        chipBox.add_child(dot);
        chipBox.add_child(chip);

        const status = new St.Label({
            style_class: 'reolink-status',
            text: 'Connecting…',
            x_expand: true,
            y_expand: true,
            x_align: Clutter.ActorAlign.CENTER,
            y_align: Clutter.ActorAlign.CENTER,
        });

        const frame = new St.Widget({
            style_class: 'reolink-tile',
            layout_manager: new Clutter.BinLayout(),
            width,
            height,
        });
        frame.add_child(image);
        frame.add_child(status);
        frame.add_child(chipBox);

        const button = new St.Button({child: frame, style_class: 'reolink-tile-button', can_focus: true});
        button.connect('clicked', () => this._open('show-camera', GLib.Variant.new_string(cam.id)));
        return {button, frame, image, chip, dot, status, width, height, mtime: 0, hasFrame: false};
    }

    // ── Preview refresh ─────────────────────────────────────────

    _startPreview() {
        this._stopPreview();
        if (!this._running)
            return;
        this._activate('preview-keepalive');
        this._keepaliveId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, KEEPALIVE_SECONDS, () => {
            this._activate('preview-keepalive');
            return GLib.SOURCE_CONTINUE;
        });
        this._refreshId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, REFRESH_MS, () => {
            this._refreshFrames();
            return GLib.SOURCE_CONTINUE;
        });
        this._refreshFrames();
    }

    _stopPreview() {
        for (const name of ['_keepaliveId', '_refreshId']) {
            if (this[name]) {
                GLib.source_remove(this[name]);
                this[name] = 0;
            }
        }
    }

    _refreshFrames() {
        const dir = GLib.build_filenamev([GLib.get_user_runtime_dir(), 'reolink-desktop', 'preview']);
        const scale = St.ThemeContext.get_for_stage(global.stage).scale_factor;
        for (const [id, tile] of this._tiles) {
            const path = GLib.build_filenamev([dir, `${id}.jpg`]);
            try {
                const info = Gio.File.new_for_path(path).query_info(
                    'time::modified,time::modified-usec', Gio.FileQueryInfoFlags.NONE, null);
                const mtime = info.get_modification_date_time().to_unix_usec();
                if (mtime === tile.mtime)
                    continue;
                tile.mtime = mtime;
                const pixbuf = GdkPixbuf.Pixbuf.new_from_file_at_scale(
                    path, tile.width * scale, -1, true);
                if (!tile.hasFrame) {
                    // Match the tile to the camera's aspect ratio (e.g. 4:3).
                    tile.height = Math.round(tile.width * pixbuf.height / pixbuf.width);
                    tile.frame.height = tile.height;
                }
                tile.image.set_content(this._toContent(pixbuf));
                tile.hasFrame = true;
                tile.status.visible = false;
            } catch {
                // No frame yet (camera connecting) — keep the placeholder.
            }
        }
    }

    _toContent(pixbuf) {
        const context = global.stage.context.get_backend().get_cogl_context();
        const content = St.ImageContent.new_with_preferred_size(pixbuf.width, pixbuf.height);
        content.set_bytes(
            context,
            pixbuf.read_pixel_bytes(),
            pixbuf.has_alpha ? Cogl.PixelFormat.RGBA_8888 : Cogl.PixelFormat.RGB_888,
            pixbuf.width,
            pixbuf.height,
            pixbuf.rowstride);
        return content;
    }

    // ── Helpers ─────────────────────────────────────────────────

    _button(iconName, label, callback) {
        const box = new St.BoxLayout({orientation: Clutter.Orientation.VERTICAL, x_expand: true});
        box.add_child(new St.Icon({icon_name: iconName, style_class: 'reolink-button-icon'}));
        box.add_child(new St.Label({text: label, x_align: Clutter.ActorAlign.CENTER}));
        const button = new St.Button({
            child: box,
            style_class: 'button reolink-button',
            x_expand: true,
            can_focus: true,
        });
        button.connect('clicked', callback);
        return button;
    }

    _onDestroy() {
        this._contextMenu?.destroy();
        this._contextMenu = null;
        this._stopPreview();
        this._cancelFocusWatch();
        this._disconnectActions();
        if (this._watchId) {
            Gio.bus_unwatch_name(this._watchId);
            this._watchId = 0;
        }
        if (this._ownId) {
            Gio.bus_unown_name(this._ownId);
            this._ownId = 0;
        }
    }
});

export default class ReolinkDesktopExtension extends Extension {
    enable() {
        this._indicator = new ReolinkIndicator();
        Main.panel.addToStatusArea(this.uuid, this._indicator);
    }

    disable() {
        this._indicator?.destroy();
        this._indicator = null;
    }
}

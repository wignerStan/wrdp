import json
import os
import py_compile
import subprocess
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

import yaml


ROOT = Path(__file__).resolve().parents[1]
CONTRIB = ROOT / "vendor/wrdp-compositor/contrib/wrdp"
SYSTEM_PROFILE = ROOT / "ground-init.system.yaml"
USER_PROFILE = ROOT / "ground-init.user.yaml"


class DesktopProvisioningTests(unittest.TestCase):
    def test_profiles_are_bounded_and_sources_exist(self):
        system = yaml.safe_load(SYSTEM_PROFILE.read_text(encoding="utf-8"))
        user = yaml.safe_load(USER_PROFILE.read_text(encoding="utf-8"))
        self.assertEqual(
            set(system), {"packages", "directories", "copy_files", "copy_trees"}
        )
        self.assertEqual(set(user), {"directories", "write_files"})
        self.assertNotIn("runcmd", system)
        self.assertNotIn("runcmd", user)

        required = {
            "waybar",
            "dbus-daemon",
            "foot",
            "util-linux",
            "fuzzel",
            "swaybg",
            "thunar",
            "gvfs",
            "gvfs-backends",
            "tumbler",
            "mako-notifier",
            "libnotify-bin",
            "python3-yaml",
            "pipewire", "pipewire-bin", "pipewire-pulse", "wireplumber",
        }
        self.assertTrue(required.issubset(set(system["packages"])))
        forbidden = {"xfce4-session", "xfce4-panel", "xfwm4", "xfdesktop4"}
        self.assertTrue(forbidden.isdisjoint(set(system["packages"])))

        source_root = str(ROOT)
        for item in system["copy_files"] + system["copy_trees"]:
            source = Path(item["source"].replace("${WRDP_SOURCE_DIR}", source_root))
            if source == Path("/usr/share/pipewire/pipewire.conf"):
                # Runtime provisioning installs this package before copying it.
                self.assertIn("pipewire", system["packages"])
                self.assertEqual(item["destination"], "/etc/wrdp/pipewire/pipewire.conf")
                self.assertEqual(item["permissions"], "0644")
            else:
                self.assertTrue(source.exists(), source)

    def test_assets_parse_and_autostart_has_one_owner_each(self):
        waybar = json.loads((CONTRIB / "waybar/config.jsonc").read_text(encoding="utf-8"))
        expected_left = ["custom/launcher", "custom/files", "custom/terminal", "wlr/taskbar"]
        expected_right = ["tray", "clock", "custom/session"]
        self.assertEqual(waybar["modules-left"], expected_left)
        self.assertEqual(waybar["modules-right"], expected_right)

        ET.parse(CONTRIB / "labwc/menu.xml")
        ET.parse(CONTRIB / "labwc/rc.xml")
        autostart = (CONTRIB / "labwc/autostart").read_text(encoding="utf-8")
        self.assertNotIn("wlr-randr", autostart)
        self.assertNotIn("foot --", autostart)
        self.assertEqual(autostart.count("/usr/lib/wrdp/wrdp-desktop-session"), 1)
        self.assertIn("setsid /usr/lib/wrdp/wrdp-desktop-session", autostart)
        self.assertIn("--compositor-pid \"$WRDP_COMPOSITOR_PID\"", autostart)
        self.assertIn("--compositor-start-ticks \"$WRDP_COMPOSITOR_START_TICKS\"", autostart)
        self.assertNotIn("pgrep", autostart)
        shutdown = CONTRIB / "labwc/shutdown"
        subprocess.run(["sh", "-n", str(shutdown)], check=True)
        shutdown_text = shutdown.read_text(encoding="utf-8")
        self.assertIn("wrdp-desktop-session --stop", shutdown_text)
        self.assertIn("--compositor-pid \"$WRDP_COMPOSITOR_PID\"", shutdown_text)
        self.assertIn("--compositor-start-ticks \"$WRDP_COMPOSITOR_START_TICKS\"", shutdown_text)
        supervisor = (CONTRIB / "bin/wrdp-desktop-session").read_text(encoding="utf-8")
        self.assertIn("dbus-run-session", supervisor)
        self.assertIn("desktop-session.bus", supervisor)
        self.assertIn("desktop-session.state", supervisor)
        self.assertIn("desktop-session.stop", supervisor)
        self.assertIn("desktop startup cancelled by compositor shutdown", supervisor)
        self.assertIn("remove_generation_state", supervisor)
        self.assertIn("kill -TERM -- \"-$supervisor_pid\"", supervisor)
        self.assertIn("kill -KILL -- \"-$supervisor_pid\"", supervisor)
        self.assertIn("process_group_alive \"$supervisor_pid\"", supervisor)
        self.assertIn("desktop process group did not terminate", supervisor)
        self.assertIn("mv -f \"$temporary\" \"$bus_file\"", supervisor)
        for process in ("swaybg", "mako", "waybar", "pipewire", "pipewire-pulse", "wireplumber"):
            self.assertEqual(supervisor.count(f"command -v {process} "), 1)
        for forbidden in ("xfce4-session", "xfce4-panel", "xfwm4", "xfdesktop"):
            self.assertNotIn(forbidden, autostart)

    def test_scripts_are_syntactically_valid_and_actions_are_whitelisted(self):
        action = CONTRIB / "bin/wrdp-desktop-action"
        supervisor = CONTRIB / "bin/wrdp-desktop-session"
        wallpaper = CONTRIB / "bin/wrdp-wallpaper"
        subprocess.run(["sh", "-n", str(action)], check=True)
        subprocess.run(["bash", "-n", str(supervisor)], check=True)
        py_compile.compile(str(wallpaper), doraise=True)
        for script in (action, supervisor, wallpaper):
            self.assertTrue(os.access(script, os.X_OK), script)

        text = action.read_text(encoding="utf-8")
        self.assertNotIn("eval", text)
        self.assertIn("desktop-session.bus", text)
        self.assertIn("valid_compositor_pid", text)
        self.assertIn("mismatched desktop session bus address", text)
        compositor_main = (
            ROOT / "vendor/wrdp-compositor/src/main.c"
        ).read_text(encoding="utf-8")
        self.assertIn("WRDP_COMPOSITOR_START_TICKS", compositor_main)
        session_config = (
            ROOT / "vendor/wrdp-compositor/src/config/session.c"
        ).read_text(encoding="utf-8")
        self.assertIn("WRDP_COMPOSITOR_START_TICKS", session_config)

        makefile = (ROOT / "Makefile").read_text(encoding="utf-8")
        for package in ("libwlroots-0.18-dev", "libxml2-dev", "wayland-protocols"):
            self.assertIn(package, makefile)

        for action_name in (
            "launcher",
            "files",
            "terminal",
            "applications",
            "reconfigure",
            "disconnect",
            "session",
        ):
            self.assertIn(action_name, text)


if __name__ == "__main__":
    unittest.main()

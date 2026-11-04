"""Hardware checks must use only a device the operator selected."""

import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from scenarios import unsupported


class RenderNodeTests(unittest.TestCase):
    def test_unselected_gpu_is_skipped_without_discovering_devices(self):
        h = Mock()
        h.vmlab.return_value = SimpleNamespace(code=1, text="missing GPU address")
        with patch.dict(unsupported.os.environ, {}, clear=True), \
                patch.object(unsupported, "scratch_lab"), \
                patch.object(unsupported.pathlib.Path, "glob", return_value=[Path("/dev/dri/renderD128")]) as discover, \
                patch.object(unsupported, "render_driver", return_value="amdgpu"), \
                patch.object(unsupported, "declare_host_gpu"), \
                patch.object(unsupported, "gpu_boot") as boot:
            unsupported.gpu(h)
        boot.assert_not_called()
        discover.assert_not_called()
        h.skip.assert_called_once()
        self.assertIn("VMLAB_E2E_RENDER_NODE", h.skip.call_args.args[1])

    def test_selected_gpu_is_the_only_device_used(self):
        h = Mock()
        h.vmlab.return_value = SimpleNamespace(code=1, text="missing GPU address")
        node = Path("/dev/dri/renderD129")
        with patch.dict(unsupported.os.environ, {"VMLAB_E2E_RENDER_NODE": str(node)}, clear=True), \
                patch.object(unsupported, "scratch_lab"), \
                patch.object(Path, "is_char_device", return_value=True), \
                patch.object(Path, "glob") as discover, \
                patch.object(unsupported, "render_driver", return_value="amdgpu"), \
                patch.object(unsupported, "declare_host_gpu") as declare, \
                patch.object(unsupported, "gpu_boot") as boot:
            unsupported.gpu(h)
        discover.assert_not_called()
        declare.assert_called_once_with(h, node)
        self.assertEqual(boot.call_args.args[:3], (h, [node], node))
        declare.return_value.assert_called_once_with()

    def test_invalid_selected_gpu_fails_without_booting(self):
        for selected in ("/dev/null", "/dev/dri/renderD999"):
            with self.subTest(selected=selected):
                h = Mock()
                h.vmlab.return_value = SimpleNamespace(code=1, text="missing GPU address")
                with patch.dict(unsupported.os.environ, {"VMLAB_E2E_RENDER_NODE": selected}, clear=True), \
                        patch.object(unsupported, "scratch_lab"), \
                        patch.object(Path, "is_char_device", return_value=False), \
                        patch.object(unsupported, "gpu_boot") as boot:
                    unsupported.gpu(h)
                boot.assert_not_called()
                self.assertEqual(h.ok.call_args.args[:2], ("vm.hw.gpu", False))


if __name__ == "__main__":
    unittest.main()

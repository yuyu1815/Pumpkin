import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import environment_probe as probe


class EnvironmentProbeTests(unittest.TestCase):
    def test_known_client_hash_and_size_are_declared(self):
        self.assertEqual(probe.EXPECTED["client_bytes"], 39193383)
        self.assertEqual(probe.EXPECTED["client_sha1"], "2dc72797acbc1b63fc16a11c4ac393605f453754")
        self.assertEqual(probe.EXPECTED["server_sha1"], "823e2250d24b3ddac457a60c92a6a941943fcd6a")

    def test_probe_only_reads_expected_metadata_and_marks_missing_server(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / ".minecraft"
            version = root / "versions" / "26.2"
            version.mkdir(parents=True)
            client = version / "26.2.jar"
            client.write_bytes(b"synthetic")
            (version / "26.2.json").write_text(
                json.dumps(
                    {
                        "id": "26.2",
                        "javaVersion": {"majorVersion": 25},
                        "downloads": {
                            "client": {"size": 9, "sha1": "not-official"},
                            "server": {"size": probe.EXPECTED["server_bytes"], "sha1": probe.EXPECTED["server_sha1"]},
                        },
                    }
                ),
                encoding="utf-8",
            )
            with patch.dict(os.environ, {"APPDATA": str(root.parent), "LOCALAPPDATA": str(root.parent)}, clear=False):
                result = probe.collect_evidence(
                    repo=None,
                    minecraft_dir=root,
                    launcher_exe=root / "missing-launcher.exe",
                    run_java=False,
                )
            self.assertFalse(result["official_server_artifact"]["present"])
            self.assertFalse(result["official_client_installation"]["client_jar"]["sha1_match"])
            self.assertIn("official 26.2 server.jar is not present", " ".join(result["blockers"]))
            self.assertEqual(result["safety"]["authentication_or_session_data"], "not read or logged")

    def test_redaction_does_not_emit_home_path(self):
        with patch.dict(os.environ, {"USERPROFILE": "C:\\PrivateUser", "APPDATA": "C:\\PrivateUser\\AppData"}, clear=False):
            value = probe._redact(Path("C:/PrivateUser/AppData/.minecraft/26.2.jar"))
        self.assertNotIn("PrivateUser", value)
        self.assertTrue(value.startswith("<APPDATA>"))


if __name__ == "__main__":
    unittest.main()

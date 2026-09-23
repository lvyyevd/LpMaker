"""离线验证运维脚本的先后顺序；所有命令均为临时目录内的替身，不连接账户。"""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


class SwitchScriptTest(unittest.TestCase):
    def run_case(self, scenario):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name in ("scripts", "config", "target/release", "bin"):
                (root / name).mkdir(parents=True)
            shutil.copy(ROOT / "scripts/switch-robinhood-dd5.sh", root / "scripts")
            (root / "config/local.toml").write_text("# fake configuration\n")
            (root / "run.log").write_text("old log retained\n")

            def executable(name, text):
                p = root / name
                p.write_text(f"#!{sys.executable}\n" + text)
                p.chmod(0o700)

            executable("bin/uname", 'print("Linux")\n')
            executable("bin/flock", "raise SystemExit(0)\n")
            executable("bin/readlink", "import os, sys\nprint(os.path.realpath(sys.argv[-1]))\n")
            executable("bin/cargo", "import os\nraise SystemExit(41 if os.environ['SCENARIO']=='build_failure' else 0)\n")
            executable("target/release/lp-maker", '''import json, os, sys, time
from pathlib import Path
args = sys.argv[1:]
with Path("calls.jsonl").open("a") as f: f.write(json.dumps(args)+"\\n")
if "check" in args:
    print("data/state")
elif "switch-robinhood-dd5" in args:
    if "--execute" in args and os.environ["SCENARIO"] == "exit_failure":
        print("unconfirmed receipt; state retained")
        raise SystemExit(42)
    print("fake plan / confirmed exit")
elif "run" in args:
    Path("runner.pid").write_text(str(os.getpid()))
    print("new runner", flush=True)
    time.sleep(60)
elif "health" in args:
    raise SystemExit(0 if Path("runner.pid").exists() else 1)
else:
    raise SystemExit(99)
''')
            env = dict(os.environ, PATH=str(root / "bin") + os.pathsep + os.environ["PATH"], SCENARIO=scenario)
            try:
                result = subprocess.run(["bash", "scripts/switch-robinhood-dd5.sh"], cwd=root,
                                        env=env, capture_output=True, text=True, errors="replace", timeout=20)
                calls_path = root / "calls.jsonl"
                calls = [json.loads(line) for line in calls_path.read_text().splitlines()] if calls_path.exists() else []
                return result, calls, (root / "run.log").read_text(), list((root / "data/operator-logs").glob("run-before*"))
            finally:
                if (root / "runner.pid").exists():
                    try:
                        os.kill(int((root / "runner.pid").read_text()), signal.SIGTERM)
                    except ProcessLookupError:
                        pass

    def test_build_failure_never_runs_exit_or_new_strategy(self):
        result, calls, log, _ = self.run_case("build_failure")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, [])
        self.assertIn("old log", log)

    def test_exit_failure_never_clears_log_or_starts_new_strategy(self):
        result, calls, log, archived = self.run_case("exit_failure")
        self.assertEqual(result.returncode, 42, result.stdout + result.stderr)
        self.assertFalse(any("run" in c for c in calls))
        self.assertIn("old log", log)
        self.assertEqual(archived, [])

    def test_success_previews_exits_then_starts_and_checks_health(self):
        result, calls, log, archived = self.run_case("success")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        preview = next(i for i, c in enumerate(calls) if "switch-robinhood-dd5" in c and "--execute" not in c)
        close = next(i for i, c in enumerate(calls) if "switch-robinhood-dd5" in c and "--execute" in c)
        start = next(i for i, c in enumerate(calls) if "run" in c)
        self.assertLess(preview, close)
        self.assertLess(close, start)
        self.assertTrue(any("health" in c for c in calls[start + 1:]))
        self.assertIn("new runner", log)
        self.assertEqual(len(archived), 1)


if __name__ == "__main__":
    unittest.main()

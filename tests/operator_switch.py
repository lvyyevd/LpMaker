"""离线验证后台会话、信号隔离和运维顺序；临时替身不连接账户。"""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]


class SwitchScriptTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.foreground = []
        for name in ("scripts/lib", "config", "target/release", "bin", "data/state"):
            (self.root / name).mkdir(parents=True)
        for name in ("switch-robinhood-dd5.sh", "start-background.sh", "lib/background.sh"):
            shutil.copy(ROOT / "scripts" / name, self.root / "scripts" / name)
        (self.root / "config/local.toml").write_text("# fake configuration\n")
        (self.root / "run.log").write_text("old log retained\n")
        (self.root / "data/state/checkpoint.json").write_text('{"existing":"keep"}\n')
        self.executable("bin/uname", 'print("Linux")\n')
        self.executable("bin/readlink", "import os, sys\nprint(os.path.realpath(sys.argv[-1]))\n")
        self.executable("bin/cargo", "import os\nraise SystemExit(41 if os.environ['SCENARIO']=='build_failure' else 0)\n")
        # macOS 没有 util-linux CLI，用同一个 POSIX setsid 系统调用验证真实会话隔离。
        if shutil.which("setsid") is None:
            self.executable("bin/setsid", '''import os, sys
assert sys.argv[1] == "--fork"
if os.fork(): os._exit(0)
os.setsid()
os.execvp(sys.argv[2], sys.argv[2:])
''')
        if shutil.which("flock") is None:
            self.executable("bin/flock", '''import fcntl, os, subprocess, sys, time
args = sys.argv[1:]
wait = 0 if args[0] == "-n" else float(args[1])
args = args[1:] if args[0] == "-n" else args[2:]
file = None
if args[0].isdigit(): fd = int(args[0])
else:
    file = open(args[0], "a")
    fd = file.fileno()
end = time.monotonic() + wait
while True:
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        break
    except BlockingIOError:
        if time.monotonic() >= end: raise SystemExit(1)
        time.sleep(0.01)
if len(args) > 1: raise SystemExit(subprocess.call(args[1:]))
''')
        self.executable("target/release/lp-maker", '''import json, os, signal, sys, time
from pathlib import Path
args = sys.argv[1:]
with Path("calls.jsonl").open("a") as f: f.write(json.dumps(args)+"\\n")
def on_signal(number, _):
    Path("unexpected-signal").write_text(str(number))
    raise SystemExit(128 + number)
# 和 Tokio 一样显式注册 SIGINT，不依赖 shell 继承的 SIG_IGN。
signal.signal(signal.SIGINT, on_signal)
signal.signal(signal.SIGHUP, on_signal)
if "check" in args:
    print("data/state")
elif "switch-robinhood-dd5" in args:
    if "--execute" in args:
        Path("exit-ready").write_text(json.dumps({"pid":os.getpid(),"sid":os.getsid(0)}))
        if os.environ["SCENARIO"] == "exit_failure":
            print("unconfirmed receipt; state retained")
            raise SystemExit(42)
        if os.environ["SCENARIO"] == "slow_exit":
            while not Path("allow-exit").exists(): time.sleep(0.02)
    print("fake plan / confirmed exit")
elif "run" in args:
    Path("runner.json").write_text(json.dumps({"pid":os.getpid(),"sid":os.getsid(0),"pgid":os.getpgrp()}))
    print("new runner", flush=True)
    time.sleep(120)
elif "health" in args:
    raise SystemExit(0 if Path("runner.json").exists() else 1)
else:
    raise SystemExit(99)
''')
        self.env = dict(os.environ, PATH=str(self.root / "bin") + os.pathsep + os.environ["PATH"], SCENARIO="success")

    def executable(self, name, text):
        p = self.root / name
        p.write_text(f"#!{sys.executable}\n" + text)
        p.chmod(0o700)

    def wait_for(self, predicate, seconds=10):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if predicate(): return
            time.sleep(0.03)
        self.fail("timed out waiting for mock workflow")

    def calls(self):
        p = self.root / "calls.jsonl"
        return [json.loads(line) for line in p.read_text().splitlines()] if p.exists() else []

    def run_worker(self, scenario):
        env = dict(self.env, SCENARIO=scenario)
        return subprocess.run(["bash", "scripts/switch-robinhood-dd5.sh", "--worker"], cwd=self.root,
                              env=env, capture_output=True, text=True, errors="replace", timeout=20)

    def foreground_shell(self, script, scenario="success"):
        # 保留原终端的前台进程组，用 killpg 模拟 Ctrl+C/SSH 挂断。
        # shell 忽略信号以便同一测试连续发送 INT/HUP；交易替身主动注册二者。
        command = f'trap "" INT HUP; bash {script}; touch launcher-returned; while :; do sleep 1; done'
        process = subprocess.Popen(["bash", "-c", command], cwd=self.root,
                                   env=dict(self.env, SCENARIO=scenario), start_new_session=True,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.foreground.append(process)
        return process

    def assert_survives_terminal_signals(self, foreground_pid, child_pid):
        self.assertNotEqual(os.getsid(child_pid), foreground_pid)
        self.assertNotEqual(os.getpgid(child_pid), foreground_pid)
        for signum in (signal.SIGINT, signal.SIGHUP):
            os.killpg(foreground_pid, signum)
            time.sleep(0.1)
            os.kill(child_pid, 0)
            self.assertFalse((self.root / "unexpected-signal").exists())

    def tearDown(self):
        # 只终止此临时测试创建的进程组，绝不匹配真实 lp-maker 进程。
        for process in self.foreground:
            try: os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError: pass
            process.wait(timeout=3)
        for record in (self.root / "data/operator-logs").glob("background.*/pid"):
            try:
                pid = int(record.read_text())
                if os.getsid(pid) == pid: os.killpg(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        self.tmp.cleanup()

    def test_build_failure_never_runs_exit_or_new_strategy(self):
        result = self.run_worker("build_failure")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls(), [])
        self.assertIn("old log", (self.root / "run.log").read_text())

    def test_exit_failure_never_clears_log_or_starts_new_strategy(self):
        result = self.run_worker("exit_failure")
        self.assertEqual(result.returncode, 42, result.stdout + result.stderr)
        self.assertFalse(any("run" in c for c in self.calls()))
        self.assertIn("old log", (self.root / "run.log").read_text())
        self.assertEqual(list((self.root / "data/operator-logs").glob("run-before*")), [])

    def test_success_previews_exits_then_starts_and_checks_health(self):
        result = self.run_worker("success")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = self.calls()
        preview = next(i for i, c in enumerate(calls) if "switch-robinhood-dd5" in c and "--execute" not in c)
        close = next(i for i, c in enumerate(calls) if "switch-robinhood-dd5" in c and "--execute" in c)
        start = next(i for i, c in enumerate(calls) if "run" in c)
        self.assertLess(preview, close)
        self.assertLess(close, start)
        self.assertTrue(any("health" in c for c in calls[start + 1:]))
        self.assertIn("new runner", (self.root / "run.log").read_text())
        self.assertEqual(len(list((self.root / "data/operator-logs").glob("run-before*"))), 1)
        # 服务不能继承运维互斥锁，否则下一次正常运维永远无法进入。
        import fcntl
        with (self.root / "data/exit-reset.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def test_ctrl_c_and_hangup_during_exit_do_not_cancel_background_migration(self):
        foreground = self.foreground_shell("scripts/switch-robinhood-dd5.sh", "slow_exit")
        self.wait_for(lambda: (self.root / "exit-ready").exists() and (self.root / "launcher-returned").exists())
        exit_process = json.loads((self.root / "exit-ready").read_text())
        self.assert_survives_terminal_signals(foreground.pid, exit_process["pid"])
        self.assertFalse((self.root / "runner.json").exists())
        (self.root / "allow-exit").touch()
        self.wait_for(lambda: (self.root / "runner.json").exists())
        runner = json.loads((self.root / "runner.json").read_text())
        self.assertEqual(runner["pid"], runner["sid"])
        self.assertNotEqual(runner["sid"], exit_process["sid"])
        self.assert_survives_terminal_signals(foreground.pid, runner["pid"])

    def test_start_only_keeps_checkpoint_and_survives_terminal_signals(self):
        checkpoint = (self.root / "data/state/checkpoint.json").read_bytes()
        foreground = self.foreground_shell("scripts/start-background.sh")
        self.wait_for(lambda: (self.root / "runner.json").exists() and (self.root / "launcher-returned").exists())
        runner = json.loads((self.root / "runner.json").read_text())
        self.assert_survives_terminal_signals(foreground.pid, runner["pid"])
        self.assertEqual((self.root / "data/state/checkpoint.json").read_bytes(), checkpoint)
        self.assertFalse(any("switch-robinhood-dd5" in c for c in self.calls()))
        self.assertIn("old log", (self.root / "run.log").read_text())
        self.assertIn("new runner", (self.root / "run.log").read_text())

    def test_start_only_blocks_unfinished_migration_without_sending_actions(self):
        marker = self.root / "data/state/strategy_migration.json"
        marker.write_text('{"stage":"pending"}')
        result = subprocess.run(["bash", "scripts/start-background.sh"], cwd=self.root,
                                env=self.env, capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any("run" in c for c in self.calls()))
        self.assertTrue(marker.exists())


if __name__ == "__main__":
    unittest.main()

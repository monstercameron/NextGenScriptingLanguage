#!/usr/bin/env python3
"""Exercise actual terminal bytes, keys, signals and worker cleanup without GitHub."""
import errno, fcntl, json, os, pathlib, pty, select, signal, struct, subprocess, sys, tempfile, termios, time
project=pathlib.Path(__file__).resolve().parents[1]
binary=str(pathlib.Path(sys.argv[1]).resolve())
source=(project/'examples/github-stars.program').read_text()
class Session:
    def __init__(self, directory, initial='morluto/rea', source_text=None):
        self.root=pathlib.Path(directory)
        gh=self.root/'gh'; gh.write_text((project/'tests/fixtures/gh.py').read_text()); gh.chmod(0o755)
        script=self.root/'ui.program'; script.write_text((source if source_text is None else source_text).replace('with value "morluto/rea"',f'with value "{initial}"'))
        self.master,self.slave=pty.openpty()
        self.before=termios.tcgetattr(self.slave)
        fcntl.ioctl(self.slave,termios.TIOCSWINSZ,struct.pack('HHHH',30,100,0,0))
        env=dict(os.environ,TERM='xterm-256color',PATH=str(self.root)+':'+os.environ['PATH'],GH_FIXTURE_DIR=str(self.root),GH_FIXTURE_MODE='normal')
        self.child=subprocess.Popen([binary,str(script),'--manifest-dir',str(project/'manifests')],cwd=self.root,env=env,stdin=self.slave,stdout=self.slave,stderr=self.slave)
        self.data=b''
    def pump(self):
        if select.select([self.master],[],[],0.02)[0]:
            try: chunk=os.read(self.master,65536)
            except OSError as e:
                if e.errno==errno.EIO:return
                raise
            self.data+=chunk
            if b'\x1b[6n' in chunk: os.write(self.master,b'\x1b[1;1R')
    def wait(self, predicate, seconds=8):
        deadline=time.monotonic()+seconds
        while not predicate():
            self.pump()
            if time.monotonic()>deadline: raise AssertionError(self.data[-6000:])
            if self.child.poll() is not None and not predicate(): raise AssertionError(('early exit',self.child.returncode,self.data[-2000:]))
        for _ in range(5):self.pump()
    def calls(self):
        path=self.root/'calls.jsonl'
        return [json.loads(v) for v in path.read_text().splitlines()] if path.exists() else []
    def send(self, value): os.write(self.master,value)
    def finish(self, sig=None):
        if sig:self.child.send_signal(sig)
        else:self.send(b'\x11')
        self.wait(lambda:self.child.poll() is not None)
        assert self.child.returncode==0,(self.child.returncode,self.data[-2000:])
        assert b'\x1b[?1049h' in self.data and b'\x1b[?1049l' in self.data
        assert termios.tcgetattr(self.slave)==self.before,'terminal settings not restored'
    def close(self):
        if self.child.poll() is None:self.child.kill();self.child.wait()
        os.close(self.master);os.close(self.slave)

def case(initial, body, source_text=None):
    with tempfile.TemporaryDirectory(prefix='execute-ui-pty-') as directory:
        s=Session(directory,initial,source_text)
        try:body(s)
        finally:s.close()

def interaction(s):
    s.wait(lambda:len(s.calls())>=1 and b'Live data.' in s.data)
    assert b'history' in s.data and b'session seconds' in s.data
    s.send(b'\x12');s.wait(lambda:len(s.calls())>=2)
    s.send(b'\x15cli/cli\r');s.wait(lambda:len(s.calls())>=3 and s.calls()[-1][5]=='cli/cli')
    # Focus saved list, select cli/cli, and submit its component message.
    s.send(b'\t\t\x1b[B\r');s.wait(lambda:len(s.calls())>=4)
    s.send(b'\x1b[Z\x1b[Z\x15fail/repo\r');s.wait(lambda:b'fixture permission' in s.data and b'denied' in s.data)
    fcntl.ioctl(s.slave,termios.TIOCSWINSZ,struct.pack('HHHH',24,80,0,0));s.child.send_signal(signal.SIGWINCH)
    for _ in range(10):s.pump()
    s.finish()
def cancellation(s):
    s.wait(lambda:(s.root/'worker.pid').exists())
    pid=int((s.root/'worker.pid').read_text());started=time.monotonic()
    s.finish()
    assert time.monotonic()-started<3,'quit blocked on slow applet'
    try:os.kill(pid,0)
    except ProcessLookupError:pass
    else:raise AssertionError('owned applet survived exit')
def switching(s):
    s.wait(lambda:(s.root/'worker.pid').exists())
    s.send(b'\x15cli/cli\r');s.wait(lambda:len(s.calls())>=2 and b'Live data.' in s.data)
    assert s.calls()[-1][5]=='cli/cli'
    assert b'Fixture' in s.data and b'description' in s.data
    s.finish()
def termination(s):
    s.wait(lambda:b'Live data.' in s.data)
    s.finish(signal.SIGTERM)
if __name__=='__main__':
    case('morluto/rea',interaction)
    case('slow/repo',cancellation)
    case('slow/repo',switching)
    case('morluto/rea',termination)
    quit_handler='  whenever key \"Ctrl-Q\" is pressed\n    close the screen\n\n'
    busy_quit=source.replace(quit_handler,'').replace('define a program named tracker\n','define a program named tracker\n'+quit_handler)
    case('slow/repo',cancellation,busy_quit)
    print('5 PTY scenarios passed: keys, resize, error display, cancellation, switching, SIGTERM restoration, script quit in busy component')

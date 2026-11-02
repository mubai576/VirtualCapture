"""Exercise shipped browser logic in Node's sandbox, without hardware or npm packages."""
import ast
import json
from pathlib import Path
import re
import shutil
import subprocess

import pytest

SOURCE = Path(__file__).resolve().parents[2] / "agent/vc-agent.py"


@pytest.fixture(scope="module")
def client():
    node = shutil.which("node")
    if not node:
        pytest.skip("Node.js is optional; needed to validate browser state logic")
    tree = ast.parse(SOURCE.read_text(encoding="utf-8"))
    html = next(ast.literal_eval(item.value) for item in tree.body
                if isinstance(item, ast.Assign)
                and any(isinstance(target, ast.Name) and target.id == "UI_HTML"
                        for target in item.targets))
    return node, re.search(r"<script>(.*?)</script>", html, re.S).group(1)


def check(client, assertions):
    node, script = client
    harness = """
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');
const input = JSON.parse(fs.readFileSync(0, 'utf8'));
const sandbox = vm.createContext({console,assert});
vm.runInContext(input.script, sandbox);
vm.runInContext(input.assertions, sandbox);
"""
    result = subprocess.run([node, "-e", harness], input=json.dumps({
        "script": script, "assertions": assertions
    }), text=True, capture_output=True, timeout=15)
    assert result.returncode == 0, result.stdout + result.stderr
def test_fps_resets_and_counter_regression(client):
    check(client, """
const old = {running:true,session_id:'a',uptime_s:10,sent_frames:2400};
assert(frameRate({...old,uptime_s:11,sent_frames:2640},old,'sent_frames',1) === 240);
assert(frameRate({...old,session_id:'b',sent_frames:1},old,'sent_frames',1) === null, 'new session');
assert(frameRate({...old,sent_frames:2399},old,'sent_frames',1) === null, 'counter rollback');
assert(frameRate({...old,uptime_s:1},old,'sent_frames',1) === null, 'process restart');
assert(frameRate({...old,running:false},old,'sent_frames',1) === null, 'stopped process');
assert(frameRate(old,old,'sent_frames',20) === null, 'stale polling gap');
assert(frameRate(old,old,'sent_frames',0) === null, 'zero interval');
""")


def test_offline_and_stub_metrics_stay_unknown(client):
    check(client, """
const sender = {running:true,note:'stub-no-binary',sent_frames:0};
assert(Object.keys(activeStats({online:true,sender},'sender')).length === 0, 'stub is not real data');
assert(Object.keys(activeStats({online:false,sender:{running:true}},'sender')).length === 0, 'offline');
assert(Object.keys(activeStats({online:true,sender:{running:false}},'sender')).length === 0, 'stopped');
assert(fmt(null) === '—' && fmt(undefined) === '—' && fmt(NaN) === '—', 'missing is not zero');
assert(fmt(0) === '0.00', 'real zero remains zero');
const status = {local:{online:true,config:{role:'receive',peer_url:'http://192.168.2.59:5143'}},
                peer:{online:false,url:'http://192.168.2.59:5143'}};
assert(getSides(status).capture === status.peer, 'keep offline peer card visible');
assert(getSides(status).receive === status.local);
""")


READY = """
const config = {role:'receive',adapter:'I226',peer_url:'http://192.168.2.59:5143'};
const status = {local:{online:true,config,version:'0.4.1',receiver:{running:false},sender:{running:false}},
    peer:{online:true,version:'0.4.1',product_revision:2,product_role:'capture',config:{role:'capture',adapter:'Realtek'},
          sender:{running:false},receiver:{running:false}}};
"""


def test_readiness_config_roles_version_and_unsaved_changes(client):
    check(client, READY + """
assert(readiness(status,false).canStart);
assert(!readiness(status,true).canStart, 'unsaved');
status.peer.online = false;
assert(!readiness(status,false).canStart, 'offline peer');
status.peer.online = true;
status.peer.config.role = 'receive';
assert(!readiness(status,false).canStart, 'same roles');
status.peer.config.role = 'capture';
status.peer.version = '0.4.0';
assert(!readiness(status,false).canStart, 'version mismatch');
status.peer.version = '0.4.1';
status.peer.config.adapter = '';
assert(readiness(status,false).canStart, 'capture worker chooses its own route adapter');
""")


def test_running_stubs_and_stop_during_peer_failure(client):
    check(client, READY + """
status.local.receiver = {running:true,note:'stub-no-binary'};
status.peer.sender = {running:true,note:'stub-no-binary'};
let state = readiness(status,false);
assert(state.level === 'warn' && !state.canStart && state.canStop);
assert(state.title.includes('占位进程'), 'must not claim real transport');
status.peer.online = false;
assert(readiness(status,false).canStop, 'local stop remains available');
status.local.config.role = '';
assert(readiness(status,false).canStop, 'stop survives role/config loss');
status.local.online = false;
assert(!readiness(status,false).canStop, 'no action against offline local agent');
""")


def test_errors_are_actionable_in_chinese(client):
    check(client, """
assert(friendlyError('local adapter not set (config.adapter)').includes('网卡'));
assert(friendlyError("peer role is 'receive', need 'capture'").includes('--role capture'));
assert(friendlyError('capture.batch 1..1000').includes('整数'));
assert(friendlyError('peer_url must be http://host:port').includes('地址格式'));
assert(friendlyError('unexpected diagnostic') === 'unexpected diagnostic', 'retain unknown diagnostic');
""")


def test_ip_only_and_fixed_port(client):
    check(client, """
assert(peerUrl('192.168.2.59') === 'http://192.168.2.59:5143');
for (const invalid of ['http://192.168.2.59', '192.168.2.59:18080', '999.1.1.1', '192.168.02.59', '', 'host']) {
  assert(peerUrl(invalid) === null, invalid);
}
""")


def test_cleanup_keeps_unified_stop_action_available(client):
    check(client, READY + """
status.local.vcam = {managed:true};
const state = readiness(status,false);
assert(state.canStop && !state.canStart, 'cleanup must be retryable after process exit');
""")


def test_native_exit_is_visible_and_local_stop_remains_available(client):
    check(client, READY + """
status.peer.sender = {running:false,error:'采集程序异常退出（退出码 2）'};
status.local.receiver = {running:true};
const state = readiness(status,false);
assert(state.level === 'bad' && state.detail.includes('退出码 2'));
assert(state.canStop && !state.canStart, 'failed sender still requires receiver cleanup');
""")


def test_running_processes_wait_for_complete_frame(client):
    check(client, READY + """
status.peer.sender = {running:true};
status.local.receiver = {running:true,complete:0};
let state = readiness(status,false);
assert(state.level === 'warn' && state.title.includes('等待首个完整画面'));
assert(state.canStop && !state.canStart);
status.local.receiver.complete = 1;
assert(readiness(status,false).level === 'good');
status.local.receiver.dropped = 1;
assert(readiness(status,false).level === 'warn');
assert(captureSource({mode:'pattern',pattern:'motion'},{capture:{mode:'dda'}}) === '测试图案 · motion');
assert(captureSource({mode:'dda'},{capture:{mode:'pattern'}}) === '桌面采集 · NV12');
""")


def test_pending_pipeline_blocks_start_not_stop(client):
    check(client, READY + """
for (const capture of [{width:416,height:416}, {codec:'h264'}, {codec:'hevc'}, {transform:'fit'}]) {
  status.local.config.capture = capture;
  assert(!readiness(status,false).canStart);
  assert(readiness(status,false).title.includes('尚不可运行'));
  status.local.receiver.running = true;
  assert(readiness(status,false).canStop, 'pending settings must never prevent cleanup');
  status.local.receiver.running = false;
  status.peer.sender.error = 'previous native exit';
  assert(!readiness(status,false).canStart, 'an old failure must not bypass pending-mode guard');
  delete status.peer.sender.error;
}
status.local.config.capture = {width:640,height:640,codec:'raw',transform:'crop'};
assert(readiness(status,false).canStart);
""")


FORM = """
const elements = new Map();
const element = id => {
  if (!elements.has(id)) elements.set(id,{id,value:'',options:[],textContent:'',hidden:false,disabled:false,
    append(option) {this.options.push(option);}});
  return elements.get(id);
};
globalThis.document = {getElementById:element,createElement:() => ({value:'',textContent:''})};
globalThis.clearTimeout = () => {};
globalThis.setTimeout = () => 99;
syncControls = () => {};
element('c_resolution').options = ['416x416','640x640','1280x720','1920x1080','2560x1440','custom'].map(value => ({value}));
const fullConfig = {role:'receive',adapter:'DirectNIC',peer_url:'http://192.168.2.59:5143',
  capture:{width:640,height:640,fps:240,codec:'raw',transform:'crop',bitrate_mbps:50,max_bitrate_mbps:80,
    mode:'dda',pattern:'motion',gpu_convert:true,mtu:9000,batch:69,dst_mac:'00:11:22:33:44:55'},
  transport:{headroom_pct:20},receive:{shm:'Local\\\\VirtualCaptureVcamFrame',mtu:9000,vcam_autoregister:true}};
fillConfig(fullConfig);
"""


def test_resolution_presets_and_custom_keep_hidden_settings(client):
    check(client, FORM + """
assert.equal(element('c_resolution').value,'640x640');
assert.equal(JSON.stringify(readConfig()),JSON.stringify(fullConfig));
for (const preset of ['416x416','1280x720','1920x1080','2560x1440']) {
  element('c_resolution').value = preset;
  markDirty({target:element('c_resolution')});
  const [w,h] = preset.split('x').map(Number), config = readConfig();
  assert.equal(config.capture.width,w); assert.equal(config.capture.height,h);
  assert.equal(config.capture.batch,69); assert.equal(config.capture.mtu,9000);
  assert.equal(config.capture.gpu_convert,true); assert.equal(config.receive.shm,fullConfig.receive.shm);
  assert(dirty);
}
element('c_width').value = '1024'; element('c_height').value = '768';
markDirty({target:element('c_width')});
assert.equal(element('c_resolution').value,'custom');
const custom = readConfig();
fillConfig(custom);
assert.equal(element('c_resolution').value,'custom');
assert(!dirty && readConfig().capture.width === 1024);
""")


def test_codec_controls_preserve_inactive_bitrates(client):
    check(client, FORM + """
assert(element('bitrateFields').hidden && element('c_bitrate').disabled);
element('c_bitrate').value = ''; element('c_max_bitrate').value = 'bad';
assert.equal(readConfig().capture.bitrate_mbps,50, 'hidden raw fields must not corrupt saved settings');
element('c_codec').value = 'hevc'; element('c_bitrate').value = '90'; element('c_max_bitrate').value = '120';
markDirty({target:element('c_codec')});
assert(!element('bitrateFields').hidden && !element('c_bitrate').disabled);
assert.equal(readConfig().capture.codec,'hevc');
assert.equal(readConfig().capture.max_bitrate_mbps,120);
element('c_headroom').value = '25';
assert.equal(readConfig().transport.headroom_pct,25);
fillConfig(fullConfig);
assert(!dirty && element('c_width').value === 640 && element('c_codec').value === 'raw');
""")


def test_stale_budget_cannot_overwrite_newer_draft(client):
    check(client, FORM + """
let respond;
api = () => new Promise(resolve => {respond=resolve;});
planGeneration = 7;
const request = updateStreamPlan();
planGeneration = 8;
element('planTitle').textContent = 'newer draft waiting';
respond([200,{network:{},implementation:{}}]);
request.then(() => {
  assert.equal(element('planTitle').textContent,'newer draft waiting');
  assert(!planInFlight && !element('bPlan').disabled);
});
""")


def test_budget_requests_do_not_save_or_start_and_can_force_refresh(client):
    check(client, FORM + """
const calls = [];
api = async (path,body) => {calls.push([path,body]); return [200,{tag:'plan'}];};
renderStreamPlan = plan => {assert.equal(plan.tag,'plan');};
planRefresh = true;
updateStreamPlan().then(() => {
  assert.equal(calls.length,1); assert.equal(calls[0][0],'/stream/plan');
  assert.equal(calls[0][1].refresh,true);
  assert.equal(calls[0][1].config.capture.width,640);
  assert(!planRefresh);
});
""")

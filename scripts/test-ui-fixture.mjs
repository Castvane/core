#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { createReadStream } from 'node:fs';
import { access, mkdtemp, readFile, realpath, rm, stat } from 'node:fs/promises';
import { createServer, request } from 'node:http';
import { createServer as createSocket } from 'node:net';
import { tmpdir } from 'node:os';
import { extname, join, resolve, sep } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

const [, , executable, fixture] = process.argv;
if (!executable || !fixture) {
    throw new Error('usage: node scripts/test-ui-fixture.mjs CASTVANE UI_BUNDLE_DIRECTORY');
}
const core = await realpath(executable);
const files = await realpath(fixture);
await access(join(files, 'index.html'));
const revision = await readFile(join(files, '.fixture-revision'), 'utf8').catch(() => 'unknown');
const home = await mkdtemp(join(tmpdir(), 'castvane-ui-'));
const children = [];
const username = 'fixture';
const password = 'test-only-fixture-password';
const requests = [];
const diagnostics = [];
const mime = new Map([
    ['.html', 'text/html; charset=utf-8'], ['.js', 'text/javascript'], ['.css', 'text/css'],
    ['.json', 'application/json'], ['.svg', 'image/svg+xml'], ['.png', 'image/png'],
    ['.ico', 'image/x-icon'], ['.woff2', 'font/woff2'],
]);

async function waitFor(check, seconds) {
    const deadline = Date.now() + seconds * 1000;
    do {
        const value = await check().catch(() => null);
        if (value) return value;
        await delay(100);
    } while (Date.now() < deadline);
    return null;
}

function listen(server) {
    return new Promise((ok, reject) => {
        server.once('error', reject);
        server.listen(0, '127.0.0.1', () => ok(server.address().port));
    });
}

function command(path, args, settings = {}) {
    const child = spawn(path, args, { detached: true, stdio: 'ignore', ...settings });
    children.push(child);
    child.on('error', () => {});
    return child;
}

const reserved = createSocket();
const port = await listen(reserved);
await new Promise(done => reserved.close(done));
let socket;
let ui;

try {
    const server = command(core, [], {
        cwd: home,
        env: {
            PATH: process.env.PATH,
            CORE_BIND_ADDR: `127.0.0.1:${port}`,
            CORE_DATABASE_PATH: join(home, 'core.sqlite'),
            CORE_STORAGE_DISK_DIR: join(home, 'media'),
            CORE_API_AUTH_ENABLE: 'true',
            CORE_API_AUTH_DISABLE_LOCALHOST: 'false',
            CORE_API_AUTH_USERNAME: username,
            CORE_API_AUTH_PASSWORD: password,
            CORE_HOST_AUTO: 'false',
            CORE_UPDATE_CHECK: 'false',
        },
    });
    const healthy = await waitFor(async () => {
        if (server.exitCode !== null) return null;
        return (await fetch(`http://127.0.0.1:${port}/ping`)).ok;
    }, 8);
    if (!healthy) throw new Error('Castvane fixture did not start');

    ui = createServer(async (incoming, outgoing) => {
        try {
            const url = new URL(incoming.url, 'http://127.0.0.1');
            if (['/api', '/ping'].includes(url.pathname) || url.pathname.startsWith('/api/')) {
                const proxy = request({
                    host: '127.0.0.1', port, method: incoming.method,
                    path: url.pathname + url.search, headers: incoming.headers,
                }, response => {
                    if (requests.length < 200) {
                        requests.push({
                            method: incoming.method, path: url.pathname, status: response.statusCode,
                        });
                    }
                    outgoing.writeHead(response.statusCode, response.headers);
                    response.pipe(outgoing);
                });
                proxy.on('error', () => { outgoing.statusCode = 502; outgoing.end(); });
                outgoing.on('close', () => proxy.destroy());
                incoming.pipe(proxy);
                return;
            }
            const path = resolve(files, '.' + decodeURIComponent(url.pathname));
            if (path !== files && !path.startsWith(files + sep)) {
                outgoing.writeHead(404).end();
                return;
            }
            const candidate = await realpath(path).catch(() => null);
            const entry = candidate?.startsWith(files + sep)
                && await stat(candidate).then(item => item.isFile()) ? candidate : join(files, 'index.html');
            outgoing.setHeader('Content-Type', mime.get(extname(entry)) || 'application/octet-stream');
            createReadStream(entry).on('error', () => outgoing.destroy()).pipe(outgoing);
        } catch {
            outgoing.writeHead(400).end();
        }
    });
    const origin = `http://127.0.0.1:${await listen(ui)}`;
    const browser = command(process.env.CHROMIUM || 'chromium', [
        '--headless', '--disable-gpu', '--disable-background-networking', '--lang=en-US',
        '--no-first-run', '--no-proxy-server', '--disable-dev-shm-usage',
        '--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE localhost, EXCLUDE 127.0.0.1',
        '--remote-debugging-port=0', `--user-data-dir=${join(home, 'browser')}`, 'about:blank',
    ]);
    const pages = await waitFor(async () => {
        if (browser.exitCode !== null) return null;
        const [debugPort] = (await readFile(join(home, 'browser', 'DevToolsActivePort'), 'utf8')).split('\n');
        return (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json();
    }, 10);
    if (!pages?.length) throw new Error('Chromium did not start. Run as an unprivileged user with its sandbox available.');
    socket = new WebSocket(pages[0].webSocketDebuggerUrl);
    await new Promise((ok, reject) => {
        const timer = setTimeout(() => reject(new Error('Chromium debugger timed out')), 5000);
        socket.addEventListener('open', () => { clearTimeout(timer); ok(); }, { once: true });
        socket.addEventListener('error', () => { clearTimeout(timer); reject(new Error('Chromium debugger failed')); }, { once: true });
    });
    let nextId = 0;
    const pending = new Map();
    socket.addEventListener('message', ({ data }) => {
        const reply = JSON.parse(data);
        pending.get(reply.id)?.(reply);
        if (diagnostics.length < 32 && (reply.method === 'Network.loadingFailed'
            || reply.method === 'Runtime.exceptionThrown')) {
            diagnostics.push(reply.params.errorText || reply.params.exceptionDetails?.text || reply.method);
        }
    });
    const call = (method, params = {}) => new Promise((ok, reject) => {
        const id = ++nextId;
        const timer = setTimeout(() => {
            pending.delete(id);
            reject(new Error(`Chromium command timed out: ${method}`));
        }, 5000);
        pending.set(id, ({ result, error }) => {
            clearTimeout(timer);
            pending.delete(id);
            if (error) reject(new Error(`Chromium command failed: ${method}`));
            else ok(result);
        });
        socket.send(JSON.stringify({ id, method, params }));
    });
    const evaluate = async expression => {
        const result = await call('Runtime.evaluate', { expression, returnByValue: true });
        return result.result.value;
    };
    await call('Network.enable');
    await call('Runtime.enable');
    const navigation = await call('Page.navigate', { url: origin + '/' });
    if (navigation.errorText) diagnostics.push(navigation.errorText);
    const requested = (method, path, status) => requests.some(row => (
        row.method === method && row.path === path && row.status === status
    ));
    const discovery = await waitFor(async () => requested('GET', '/api', 200), 20);
    const form = discovery && await waitFor(
        () => evaluate("Boolean(document.querySelector('input[type=password]'))"), 8,
    );
    let login = false;
    let refresh = false;
    let versionGate = false;
    let passwordInputRetainsValue = null;
    if (form) {
        const submit = async secret => {
            for (const [selector, value] of [
                ['#username, input[autocomplete=username], input[name=username]', username],
                ['input[type=password]', secret],
            ]) {
                if (!await evaluate(`Boolean(document.querySelector(${JSON.stringify(selector)}))`)) return false;
                await evaluate(`(() => {
                    const input = document.querySelector(${JSON.stringify(selector)});
                    input.focus();
                    input.select();
                })()`);
                await call('Input.insertText', { text: value });
            }
            passwordInputRetainsValue = await evaluate("Boolean(document.querySelector('input[type=password]')?.value)");
            return evaluate(`(() => {
                const form = document.querySelector('input[type=password]')?.closest('form');
                const button = document.querySelector('[name=login_local]') || form?.querySelector('button[type=submit]');
                if (button) button.click();
                else if (form) form.requestSubmit();
                return Boolean(button || form);
            })()`);
        };
        if (await submit('intentionally-invalid')) {
            await waitFor(async () => requested('POST', '/api/login', 403), 4);
            await submit(password);
            login = Boolean(await waitFor(async () => requested('POST', '/api/login', 200), 8));
        }
        if (login) {
            const banner = () => evaluate(
                "/does not support|doesn.t support|incompatible|not compatible|UI requires|unsupported (core|ffmpeg)/i.test(document.body?.innerText || '')",
            );
            versionGate = Boolean(await waitFor(async () => requested('GET', '/api/v3/config', 200) || await banner(), 5))
                && !requested('GET', '/api/v3/config', 200);
            await call('Page.reload');
            refresh = Boolean(await waitFor(async () => requested('GET', '/api/login/refresh', 200), 8));
        }
    }
    console.log(JSON.stringify({
        revision: revision.trim(),
        discovery: Boolean(discovery), loginForm: Boolean(form),
        rejectsInvalidPassword: requested('POST', '/api/login', 403),
        login, refresh, versionGate, passwordInputRetainsValue,
        dashboardConfig: requested('GET', '/api/v3/config', 200), requests, diagnostics,
    }, null, 2));
    if (!discovery || !login) process.exitCode = 1;
} finally {
    socket?.close();
    ui?.closeAllConnections();
    ui?.close();
    for (const child of children.reverse()) {
        if (child.pid && child.exitCode === null) {
            try { process.kill(-child.pid, 'SIGTERM'); } catch { /* child already exited */ }
        }
    }
    await rm(home, { recursive: true, force: true, maxRetries: 6, retryDelay: 100 });
}

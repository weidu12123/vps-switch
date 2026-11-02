<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import BrandLogo from './components/BrandLogo.vue';
import OsBadge from './components/OsBadge.vue';
import HostDetailDrawer from './components/HostDetailDrawer.vue';
import HostModal from './components/HostModal.vue';
import TerminalStage from './components/TerminalStage.vue';
import FileBrowserPanel from './components/FileBrowserPanel.vue';
import {
  globalInspectionCache,
  ROLE_META,
  seedInspectionMap,
  type HostRole,
  type HostUpsertPayload,
  type PaneMeta,
  type PublicHost,
} from './types';

type CenterStageMode = 'terminal' | 'files' | null;
type RightWingMode = 'detail' | 'files' | null;

interface OpenSessionTab {
  hostId: string;
  mode: 'terminal' | 'files';
  rightWing: RightWingMode;
  splitDirection: 'row' | 'col';
  panes: PaneMeta[];
  /// 建 pane 失败时的原因。留在标签上，让舞台能渲染成"失败 + 重试"，
  /// 而不是永远转着"正在连接"（独立窗口模式下没有标签栏，转圈就是死路）。
  error?: string | null;
}

// 独立系统终端原生窗口模式判定 (?standalone=1&hostId=...)
const urlParams = new URLSearchParams(window.location.search);
const isStandalone = ref(
  urlParams.get('standalone') === '1' || urlParams.get('mode') === 'terminal-standalone'
);
const standaloneHostId = ref<string | null>(urlParams.get('hostId'));

// 舞台全宽沉浸式体验：是否在舞台打开时展开左侧主机列表（默认收起以提供 100% 独占全宽终端）
const showSidebarInStage = ref(false);

// 顶层页面状态：'workspace'（仓库+终端+文件主工作台） | 'settings'（独立设置页，不做弹窗）
const activePage = ref<'workspace' | 'settings'>('workspace');

// 登录鉴权状态
const authChecked = ref(false);
const authEnabled = ref(false);
const authenticated = ref(true);
const loginUsername = ref('admin');
const loginPassword = ref('');
const loginError = ref('');
const loginSubmitting = ref(false);

const hosts = ref<PublicHost[]>([]);
const loading = ref(false);
const checkingAll = ref(false);
const checkingIds = ref<Set<string>>(new Set());
const paneCreationInFlight = new Set<string>();
const paneCreationCount = ref(0);
const creatingPane = computed(() => paneCreationCount.value > 0);

// 全局昼夜主题
const isDark = ref(false);
const terminalDark = ref(false);
const toast = ref<{ text: string; kind: 'ok' | 'err' } | null>(null);

// 仓库统一筛选器
const searchQuery = ref('');
const filterMenuOpen = ref(false);
const statusFilter = ref<'all' | 'online' | 'offline'>('all');
const favoriteOnly = ref(false);
const roleFilter = ref<'all' | HostRole | 'archived'>('all');
const tagFilter = ref<string>('');

// 左上角已打开的多终端/文件会话标签列表
const openTabs = ref<OpenSessionTab[]>([]);

// 主舞台与最右栏各自对应的主机与模式
const stageHostId = ref<string | null>(null);
const centerStage = ref<CenterStageMode>(null);

const detailHostId = ref<string | null>(null);
const rightWing = ref<RightWingMode>(null);

// 新增/编辑主机弹窗 与 删除确认弹窗
const modalOpen = ref(false);
const editingHost = ref<PublicHost | null>(null);
const deletingHost = ref<PublicHost | null>(null);
const searchInputRef = ref<HTMLInputElement | null>(null);

// 独立设置页状态（MCP 开关、API Key、Web 登录账号密码）
const mcpEnabled = ref(true);
const mcpRequireApiKey = ref(false);
const mcpApiKey = ref('');
const mcpEnabledTools = ref<string[]>([
  'vps_list',
  'vps_inspect',
  'vps_exec',
  'vps_terminal',
  'vps_fs',
  'vps_transfer',
  'vps_task',
]);
const mcpToolCatalog = [
  { name: 'vps_list', desc: '主机列表与概览' },
  { name: 'vps_inspect', desc: '实时深度探针' },
  { name: 'vps_exec', desc: '非交互命令执行' },
  { name: 'vps_terminal', desc: '交互式 PTY 终端' },
  { name: 'vps_fs', desc: '远程文件系统' },
  { name: 'vps_transfer', desc: '大文件流式传输' },
  { name: 'vps_task', desc: '后台任务查询与取消' },
] as const;
const showApiKey = ref(false);
const configModePreview = ref<'remote' | 'stdio'>('remote');

const settingAuthEnabled = ref(false);
const settingAdminUsername = ref('admin');
const settingAdminPassword = ref('');
const hasAdminPassword = ref(false);
const savingAuth = ref(false);
const savingMcp = ref(false);

// 命令黑名单（Agent 执行护栏）：名单原文与文件路径
const guardContent = ref('');
const guardPath = ref('');
const savingGuard = ref(false);

// MCP 调用记录（全程留痕）：外部 agent 经 MCP 通道的每一次调用
type McpCallRecord = {
  id: string;
  ts: string;
  tool: string;
  host: string | null;
  args: string;
  ok: boolean;
  durationMs: number;
  summary: string;
};
const mcpCalls = ref<McpCallRecord[]>([]);
const mcpCallHosts = ref<string[]>([]);
const mcpCallTools = ref<string[]>([]);
const loadingMcpCalls = ref(false);
// 列表只回预览，展开某一行时才按 id 取回完整参数与返回结果
const expandedCallId = ref<string | null>(null);
const callDetail = ref<{ args: string; result: string } | null>(null);
const loadingCallDetail = ref(false);
/// 详情请求的序号：快速点两行时，先发的请求后到会把后一行的正文盖掉（同 loadDir 的 navSeq）
let callDetailSeq = 0;
const MCP_CALL_LIMIT = 500;
const mcpCallFilters = ref({ from: '', to: '', host: '', tool: '', ok: '' });
const mcpCallFiltered = computed(
  () =>
    Boolean(
      mcpCallFilters.value.from ||
        mcpCallFilters.value.to ||
        mcpCallFilters.value.host ||
        mcpCallFilters.value.tool ||
        mcpCallFilters.value.ok
    )
);

const mcpStatus = ref<{
  exePath: string;
  port: number;
  env: Record<string, string>;
  claudeCodePath: string;
  claudeCodeSynced: boolean;
  codexPath: string;
  codexSynced: boolean;
} | null>(null);

// 设置页分类导航与状态
type SettingsTabKey = 'general' | 'security' | 'mcp' | 'calls' | 'about';
const activeSettingsTab = ref<SettingsTabKey>('general');

const autostartEnabled = ref(false);
const closeAction = ref<'tray' | 'exit'>('tray');
const savingAutostart = ref(false);

const appAbout = ref<{
  version: string;
  description: string;
  exePath: string;
  port: number;
  githubUrl: string;
  stack: string;
} | null>(null);

const updateStatus = ref<{
  checking: boolean;
  checked: boolean;
  hasUpdate: boolean;
  currentVersion: string;
  latestVersion: string;
  publishedAt?: string;
  releaseNotes?: string;
  releaseUrl?: string;
}>({
  checking: false,
  checked: false,
  hasUpdate: false,
  currentVersion: 'v0.1.2',
  latestVersion: 'v0.1.2',
});

// 拖拽排序
const draggingId = ref<string | null>(null);
const dragOverId = ref<string | null>(null);

let toastTimer: ReturnType<typeof setTimeout> | null = null;
function showToast(text: string, kind: 'ok' | 'err' = 'ok') {
  toast.value = { text, kind };
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    toast.value = null;
  }, 2800);
}

function applyTheme(dark: boolean) {
  isDark.value = dark;
  terminalDark.value = dark;
  document.documentElement.classList.toggle('dark', dark);
  try {
    localStorage.setItem('vps-switch-theme', dark ? 'dark' : 'light');
  } catch {
    // ignore
  }
}

function toggleTheme() {
  applyTheme(!isDark.value);
}

function hostById(id: string): PublicHost | undefined {
  return hosts.value.find((h) => h.id === id);
}

const stageHost = computed(
  () => hosts.value.find((h) => h.id === stageHostId.value) || null
);

const activeSessionTab = computed(
  () => openTabs.value.find((t) => t.hostId === stageHostId.value) || null
);

const rightWingHost = computed(
  () =>
    hosts.value.find((h) => h.id === detailHostId.value) ||
    stageHost.value ||
    null
);

const hasCenterStage = computed(
  () => Boolean(stageHost.value && centerStage.value)
);

const allTags = computed<string[]>(() => {
  const set = new Set<string>();
  for (const h of hosts.value) {
    if (h.archived && roleFilter.value !== 'archived') continue;
    for (const t of h.tags) {
      if (t.trim()) set.add(t.trim());
    }
  }
  return Array.from(set).sort();
});

const hasActiveFilter = computed(
  () =>
    statusFilter.value !== 'all' ||
    favoriteOnly.value ||
    roleFilter.value !== 'all' ||
    Boolean(tagFilter.value)
);

const activeFilterSummary = computed(() => {
  const parts: string[] = [];
  if (favoriteOnly.value) parts.push('★ 收藏');
  if (statusFilter.value === 'online') parts.push('在线');
  else if (statusFilter.value === 'offline') parts.push('离线');

  if (roleFilter.value === 'archived') parts.push('归档');
  else if (roleFilter.value !== 'all' && ROLE_META[roleFilter.value]) {
    parts.push(ROLE_META[roleFilter.value].label);
  }

  if (tagFilter.value) parts.push(`#${tagFilter.value}`);
  return parts.length ? `筛选 · ${parts.join(' · ')}` : '筛选';
});

function resetFilters() {
  statusFilter.value = 'all';
  favoriteOnly.value = false;
  roleFilter.value = 'all';
  tagFilter.value = '';
}

const filteredHosts = computed(() => {
  const q = searchQuery.value.trim().toLowerCase();
  return hosts.value.filter((h) => {
    if (roleFilter.value === 'archived') {
      if (!h.archived) return false;
    } else {
      if (h.archived) return false;
      if (roleFilter.value !== 'all' && h.role !== roleFilter.value) return false;
    }
    if (favoriteOnly.value && !h.favorited) return false;
    if (statusFilter.value === 'online' && h.status !== 'online') return false;
    if (statusFilter.value === 'offline' && h.status !== 'offline') return false;
    if (tagFilter.value && !h.tags.includes(tagFilter.value)) return false;
    if (!q) return true;
    const hay = [
      h.name,
      h.addr,
      String(h.port),
      h.username,
      h.osInfo?.prettyName || '',
      ...h.tags,
    ]
      .join(' ')
      .toLowerCase();
    return hay.includes(q);
  });
});

const onlineCount = computed(
  () => hosts.value.filter((h) => !h.archived && h.status === 'online').length
);
const offlineCount = computed(
  () => hosts.value.filter((h) => !h.archived && h.status === 'offline').length
);
const favoriteCount = computed(
  () => hosts.value.filter((h) => !h.archived && h.favorited).length
);
const totalActiveCount = computed(
  () => hosts.value.filter((h) => !h.archived).length
);

const roleFilterOptions: Array<{ key: 'all' | HostRole | 'archived'; label: string }> = [
  { key: 'all', label: '全部分类' },
  { key: 'primary', label: '主力' },
  { key: 'proxy', label: '代理' },
  { key: 'test', label: '测试' },
  { key: 'archived', label: '归档' },
];

// ==================== 登录鉴权与设置页逻辑 ====================

async function checkAuthStatus() {
  try {
    const res = await fetch('/api/auth/status');
    const data = await res.json();
    authEnabled.value = Boolean(data.authEnabled);
    authenticated.value = Boolean(data.authenticated);
    if (data.username) {
      loginUsername.value = data.username;
    }
  } catch {
    authenticated.value = true;
  } finally {
    authChecked.value = true;
  }
}

async function handleLoginSubmit() {
  loginError.value = '';
  loginSubmitting.value = true;
  try {
    const res = await fetch('/api/auth/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        username: loginUsername.value,
        password: loginPassword.value,
      }),
    });
    const data = await res.json();
    if (!res.ok || !data.ok) {
      loginError.value = data.error || '账号或密码错误';
      return;
    }
    loginPassword.value = '';
    authenticated.value = true;
    await initAuthenticatedWorkspace();
  } catch (e) {
    loginError.value = (e as Error).message || '登录请求失败';
  } finally {
    loginSubmitting.value = false;
  }
}

async function handleLogout() {
  await fetch('/api/auth/logout', { method: 'POST' });
  authenticated.value = false;
  activePage.value = 'workspace';
}

/// 所有受保护的后端请求都走这里：401 表示会话已失效，直接退回登录页。
/// 之前只有三处处理 401，其余把 401 当普通错误弹个 toast 就算了——而会话表存在内存里，
/// **服务每次重启都会清空**，所以开了登录保护时这不是边缘情况：用户会停在一个已经死掉的
/// 工作台里，点什么都没反应。返回 null 表示请求已作废，调用方 `if (!res) return;` 即可。
async function apiFetch(input: string, init?: RequestInit): Promise<Response | null> {
  const res = await fetch(input, init);
  if (res.status === 401) {
    authenticated.value = false;
    activePage.value = 'workspace';
    return null;
  }
  return res;
}

/// 子面板（文件管理器等）发现会话失效时调这里，走同一条回登录页的路
function handleSessionExpired() {
  authenticated.value = false;
  activePage.value = 'workspace';
}

async function loadSettings() {
  try {
    const res = await apiFetch('/api/settings');
    if (!res) return;
    const data = await res.json();
    if (data.ok && data.settings) {
      const s = data.settings;
      mcpEnabled.value = Boolean(s.mcpEnabled);
      mcpRequireApiKey.value = Boolean(s.mcpRequireApiKey);
      mcpApiKey.value = s.mcpApiKey || '';
      if (Array.isArray(s.mcpEnabledTools)) {
        mcpEnabledTools.value = s.mcpEnabledTools;
      }
      settingAuthEnabled.value = Boolean(s.authEnabled);
      authEnabled.value = Boolean(s.authEnabled);
      settingAdminUsername.value = s.adminUsername || 'admin';
      hasAdminPassword.value = Boolean(s.hasAdminPassword);
    }
    if (data.cliSync) {
      mcpStatus.value = data.cliSync;
    }
    if (data.autostart !== undefined) {
      autostartEnabled.value = Boolean(data.autostart);
    }
    if (data.closeAction) {
      closeAction.value = data.closeAction === 'exit' ? 'exit' : 'tray';
    }
  } catch {
    // ignore
  }
  void loadAutostartSettings();
  void loadAboutInfo();
  void loadGuardList();
}

async function loadGuardList() {
  try {
    const res = await apiFetch('/api/guard');
    if (!res) return;
    const data = await res.json();
    if (data.ok) {
      guardContent.value = data.content || '';
      guardPath.value = data.path || '';
    }
  } catch {
    // ignore
  }
}

async function saveGuardList() {
  savingGuard.value = true;
  try {
    const res = await apiFetch('/api/guard', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ content: guardContent.value }),
    });
    if (!res) return;
    const data = await res.json();
    if (!data.ok) {
      showToast(data.error || '保存命令黑名单失败', 'err');
      return;
    }
    showToast('命令黑名单已保存，立即生效');
  } catch (e) {
    showToast(`保存失败: ${(e as Error).message}`, 'err');
  } finally {
    savingGuard.value = false;
  }
}

async function loadMcpCalls() {
  loadingMcpCalls.value = true;
  try {
    const q = new URLSearchParams();
    q.set('limit', String(MCP_CALL_LIMIT));
    const f = mcpCallFilters.value;
    // 日期按使用者本地时区取整天，换算成 UTC 瞬时再交给后端做闭开区间比较
    if (f.from) q.set('from', new Date(`${f.from}T00:00:00`).toISOString());
    if (f.to) {
      const end = new Date(`${f.to}T00:00:00`);
      end.setDate(end.getDate() + 1);
      q.set('to', end.toISOString());
    }
    if (f.host) q.set('host', f.host);
    if (f.tool) q.set('tool', f.tool);
    if (f.ok) q.set('ok', f.ok);

    const res = await apiFetch(`/api/mcp-calls?${q.toString()}`);
    if (!res) return;
    const data = await res.json();
    if (data.ok) {
      mcpCalls.value = Array.isArray(data.calls) ? data.calls : [];
      mcpCallHosts.value = Array.isArray(data.hosts) ? data.hosts : [];
      mcpCallTools.value = Array.isArray(data.tools) ? data.tools : [];
    }
  } catch {
    // ignore
  } finally {
    loadingMcpCalls.value = false;
  }
}

function resetMcpCallFilters() {
  mcpCallFilters.value = { from: '', to: '', host: '', tool: '', ok: '' };
  void loadMcpCalls();
}

// 展开/收起一行：展开时才去取这条记录的完整正文
async function toggleCall(id: string) {
  if (expandedCallId.value === id) {
    expandedCallId.value = null;
    callDetail.value = null;
    return;
  }
  expandedCallId.value = id;
  callDetail.value = null;
  loadingCallDetail.value = true;
  const seq = ++callDetailSeq;
  try {
    const res = await apiFetch(`/api/mcp-calls/${encodeURIComponent(id)}`);
    if (!res) return;
    const data = await res.json();
    // 迟到的响应属于上一行，丢弃——否则展开的是 B，显示的却是 A 的参数与返回
    if (seq !== callDetailSeq) return;
    if (data.ok && data.call) {
      callDetail.value = { args: data.call.args || '', result: data.call.result || '' };
    }
  } catch {
    // ignore
  } finally {
    if (seq === callDetailSeq) loadingCallDetail.value = false;
  }
}

// 把 RFC3339 时间戳显示成本地「YYYY-MM-DD HH:MM:SS」
function formatCallTime(ts: string): string {
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return ts;
  const p = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

// 切到「调用记录」分类时自动拉取
watch(activeSettingsTab, (tab) => {
  if (tab === 'calls') {
    expandedCallId.value = null;
    callDetail.value = null;
    void loadMcpCalls();
  }
});

async function loadAutostartSettings() {
  try {
    const res = await apiFetch('/api/system/autostart');
    if (!res) return;
    const data = await res.json();
    if (data.ok) {
      autostartEnabled.value = Boolean(data.autostart);
      closeAction.value = data.closeAction === 'exit' ? 'exit' : 'tray';
    }
  } catch {
    // ignore
  }
}

async function saveAutostartSettings() {
  // 开关是乐观翻转的：注册表写失败必须把界面翻回去，不能让"已开启"骗人
  const prevAutostart = autostartEnabled.value;
  const prevCloseAction = closeAction.value;
  savingAutostart.value = true;
  try {
    const res = await apiFetch('/api/system/autostart', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        autostart: autostartEnabled.value,
        closeAction: closeAction.value,
      }),
    });
    if (!res) return;
    const data = await res.json();
    if (!data.ok) {
      autostartEnabled.value = prevAutostart;
      closeAction.value = prevCloseAction;
      showToast(data.error || '保存自启与运行设置失败', 'err');
      return;
    }
    autostartEnabled.value = Boolean(data.autostart);
    closeAction.value = data.closeAction === 'exit' ? 'exit' : 'tray';
    showToast('开机自启与后台常驻设置已生效');
  } catch (e) {
    autostartEnabled.value = prevAutostart;
    closeAction.value = prevCloseAction;
    showToast(`保存失败: ${(e as Error).message}`, 'err');
  } finally {
    savingAutostart.value = false;
  }
}

async function loadAboutInfo() {
  try {
    const res = await apiFetch('/api/system/about');
    if (!res) return;
    const data = await res.json();
    if (data.ok) {
      appAbout.value = data;
      updateStatus.value.currentVersion = data.version;
    }
  } catch {
    // ignore
  }
}

async function checkAppUpdate() {
  updateStatus.value.checking = true;
  try {
    const res = await apiFetch('/api/system/check-update', { method: 'POST' });
    if (!res) return;
    const data = await res.json();
    if (data.ok) {
      updateStatus.value = {
        checking: false,
        checked: true,
        hasUpdate: Boolean(data.hasUpdate),
        currentVersion: data.currentVersion || 'v0.1.2',
        latestVersion: data.latestVersion || 'v0.1.2',
        publishedAt: data.publishedAt,
        releaseNotes: data.releaseNotes,
        releaseUrl: data.releaseUrl,
      };
      if (data.hasUpdate) {
        showToast(`发现新版本 ${data.latestVersion}！`);
      } else {
        showToast('当前已是最新版本');
      }
    } else {
      // 后端在失败分支返回的是 error（不是 message），且带着 gh 的原始 stdout/stderr 与退出码——
      // 只回一句"检查更新失败"等于把唯一有用的排查信息丢了。
      const detail = data.error || data.stderr || '';
      updateStatus.value = {
        checking: false,
        checked: true,
        hasUpdate: false,
        currentVersion: data.currentVersion || updateStatus.value.currentVersion,
        latestVersion: data.latestVersion || updateStatus.value.currentVersion,
      };
      showToast(detail ? `检查更新失败: ${detail}` : '检查更新失败', 'err');
    }
  } catch (e) {
    showToast(`检查更新出错: ${(e as Error).message}`, 'err');
  } finally {
    updateStatus.value.checking = false;
  }
}

async function saveMcpSettings(regenerateKey = false) {
  // 同上：MCP 总开关与鉴权开关都是乐观翻转，保存失败要翻回真实状态
  const prevEnabled = mcpEnabled.value;
  const prevRequireKey = mcpRequireApiKey.value;
  savingMcp.value = true;
  try {
    const res = await apiFetch('/api/settings/mcp', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        mcpEnabled: mcpEnabled.value,
        mcpRequireApiKey: mcpRequireApiKey.value,
        regenerateKey,
      }),
    });
    if (!res) return;
    const data = await res.json();
    if (!data.ok) {
      mcpEnabled.value = prevEnabled;
      mcpRequireApiKey.value = prevRequireKey;
      showToast(data.error || '保存 MCP 设置失败', 'err');
      return;
    }
    mcpEnabled.value = Boolean(data.settings.mcpEnabled);
    mcpRequireApiKey.value = Boolean(data.settings.mcpRequireApiKey);
    mcpApiKey.value = data.settings.mcpApiKey || '';
    showToast(regenerateKey ? '已生成新的 MCP API Key' : 'MCP 设置已保存');
  } catch (e) {
    mcpEnabled.value = prevEnabled;
    mcpRequireApiKey.value = prevRequireKey;
    showToast(`保存失败: ${(e as Error).message}`, 'err');
  } finally {
    savingMcp.value = false;
  }
}

async function saveAuthSettings() {
  if (settingAdminPassword.value.trim().length > 0) {
    settingAuthEnabled.value = true;
  }
  savingAuth.value = true;
  try {
    const res = await apiFetch('/api/settings/auth', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        authEnabled: settingAuthEnabled.value,
        username: settingAdminUsername.value,
        password: settingAdminPassword.value || undefined,
      }),
    });
    if (!res) return;
    const data = await res.json();
    if (!data.ok) {
      showToast(data.error || '保存登录设置失败', 'err');
      return;
    }
    authEnabled.value = Boolean(data.settings.authEnabled);
    settingAuthEnabled.value = Boolean(data.settings.authEnabled);
    settingAdminUsername.value = data.settings.adminUsername;
    hasAdminPassword.value = Boolean(data.settings.hasAdminPassword);
    settingAdminPassword.value = '';
    showToast('访问安全与登录设置已保存');
  } catch (e) {
    showToast(`保存失败: ${(e as Error).message}`, 'err');
  } finally {
    savingAuth.value = false;
  }
}

async function triggerCliSync() {
  try {
    const res = await apiFetch('/api/mcp/status', { method: 'POST' });
    if (!res) return;
    const data = await res.json();
    if (!data.ok || !data.status) {
      showToast(data.error || '写入本机 CLI 配置失败', 'err');
      return;
    }
    mcpStatus.value = data.status;
    showToast('已同步写入本机 Claude Code 与 Codex 配置文件');
  } catch (e) {
    showToast(`写入本机 CLI 配置失败: ${(e as Error).message}`, 'err');
  }
}

const remoteHttpEndpoint = computed(() => {
  return `${window.location.origin}/mcp`;
});

const displayedMcpConfigJson = computed(() => {
  if (configModePreview.value === 'remote') {
    const entry: Record<string, unknown> = {
      type: 'http',
      url: remoteHttpEndpoint.value,
    };
    if (mcpRequireApiKey.value && mcpApiKey.value) {
      entry.headers = {
        Authorization: `Bearer ${mcpApiKey.value}`,
      };
    }
    return JSON.stringify(
      {
        mcpServers: {
          'vps-switch': entry,
        },
      },
      null,
      2
    );
  }
  return JSON.stringify(
    {
      mcpServers: {
        'vps-switch': {
          type: 'stdio',
          command: mcpStatus.value?.exePath || 'vps-switch.exe',
          args: ['--mcp-stdio'],
          env: mcpStatus.value?.env,
        },
      },
    },
    null,
    2
  );
});

function copyText(text: string, msg: string) {
  void navigator.clipboard.writeText(text);
  showToast(msg);
}

function openSettingsPage(tab?: SettingsTabKey | unknown) {
  const targetTab = typeof tab === 'string' ? (tab as SettingsTabKey) : undefined;
  if (targetTab) {
    activeSettingsTab.value = targetTab;
  }
  if (activePage.value === 'settings' && !targetTab) {
    activePage.value = 'workspace';
  } else {
    activePage.value = 'settings';
    void loadSettings();
  }
}

// ==================== 工作台与实时事件 ====================

let uiEventSource: EventSource | null = null;

function subscribeUiEvents() {
  if (uiEventSource) uiEventSource.close();
  uiEventSource = new EventSource('/api/events');
  uiEventSource.onmessage = (ev) => {
    try {
      const data = JSON.parse(ev.data);
      if (data.type === 'revealTerminal' && data.hostId && data.pane) {
        const pane = data.pane as PaneMeta;
        let tab = openTabs.value.find((t) => t.hostId === data.hostId);
        if (!tab) {
          tab = {
            hostId: data.hostId,
            mode: 'terminal',
            rightWing: null,
            splitDirection: 'row',
            panes: [pane],
          };
          openTabs.value.push(tab);
        } else {
          tab.mode = 'terminal';
          if (!tab.panes.some((p) => p.id === pane.id)) {
            tab.panes.push(pane);
          }
        }
        activePage.value = 'workspace';
        stageHostId.value = data.hostId;
        centerStage.value = 'terminal';
        detailHostId.value = data.hostId;
        const hostName = hostById(data.hostId)?.name || data.hostId;
        showToast(
          data.reason
            ? `Agent 唤起「${hostName}」终端: ${data.reason}`
            : `Agent 已唤起「${hostName}」终端供你操作`
        );
      } else if (data.type === 'hostsUpdated') {
        void fetchHosts(true);
      }
    } catch {
      // ignore
    }
  };
}

async function fetchHosts(silent = false) {
  if (!silent) loading.value = true;
  try {
    const res = await apiFetch('/api/hosts');
    if (!res) return;
    const data = await res.json();
    if (data.ok && Array.isArray(data.hosts)) {
      hosts.value = data.hosts;
      seedInspectionMap(data.inspections);
    }
  } catch (e) {
    if (!silent) showToast(`读取主机列表失败: ${(e as Error).message}`, 'err');
  } finally {
    if (!silent) loading.value = false;
  }
}

async function restoreExistingTerminals() {
  try {
    const res = await apiFetch('/api/terminals');
    if (!res || !res.ok) return;
    const data = await res.json();
    if (!data.ok || !Array.isArray(data.panes)) return;
    const byHost = new Map<string, PaneMeta[]>();
    for (const p of data.panes as PaneMeta[]) {
      if (p.exited) continue;
      const list = byHost.get(p.hostId) || [];
      list.push(p);
      byHost.set(p.hostId, list);
    }
    for (const [hostId, panes] of byHost.entries()) {
      let tab = openTabs.value.find((t) => t.hostId === hostId);
      if (!tab) {
        tab = {
          hostId,
          mode: 'terminal',
          rightWing: null,
          splitDirection: 'row',
          panes,
        };
        openTabs.value.push(tab);
      } else {
        tab.panes = panes;
      }
    }
  } catch {
    // ignore
  }
}

function sortHostsInPlace() {
  hosts.value.sort((a, b) => {
    const aLocal = a.kind === 'local' || a.id === 'local' ? 0 : 1;
    const bLocal = b.kind === 'local' || b.id === 'local' ? 0 : 1;
    if (aLocal !== bLocal) return aLocal - bLocal;
    if (a.pinned !== b.pinned) return a.pinned ? -1 : 1;
    const orderDiff = (a.sortOrder ?? 0) - (b.sortOrder ?? 0);
    if (orderDiff !== 0) return orderDiff;
    return a.name.localeCompare(b.name, undefined, { sensitivity: 'base' });
  });
}

function replaceHostInList(updated: PublicHost) {
  const idx = hosts.value.findIndex((h) => h.id === updated.id);
  if (idx >= 0) {
    hosts.value.splice(idx, 1, updated);
  } else {
    hosts.value.push(updated);
  }
  sortHostsInPlace();
}

async function checkSingleHost(host: PublicHost) {
  if (checkingIds.value.has(host.id)) return;
  const next = new Set(checkingIds.value);
  next.add(host.id);
  checkingIds.value = next;
  try {
    const res = await apiFetch(`/api/hosts/${encodeURIComponent(host.id)}/check`, {
      method: 'POST',
    });
    if (!res) return;
    const data = await res.json();
    if (data.ok && data.host) {
      replaceHostInList(data.host);
      if (data.inspection) {
        globalInspectionCache.set(data.host.id, data.inspection);
      }
      if (data.host.status === 'online') {
        showToast(`${data.host.name} 在线 (${data.host.latencyMs ?? 0} ms)`);
      } else if (data.host.status === 'unknown') {
        showToast(`${data.host.name} 连接超时，未能判定（可能只是线路慢）`, 'err');
      } else {
        showToast(`${data.host.name} 离线`, 'err');
      }
    }
  } catch (e) {
    showToast(`检测失败: ${(e as Error).message}`, 'err');
  } finally {
    const after = new Set(checkingIds.value);
    after.delete(host.id);
    checkingIds.value = after;
  }
}

async function checkAllHosts() {
  if (checkingAll.value) return;
  checkingAll.value = true;
  showToast('正在并发探索全部主机在线状态与实时硬件快照...');
  try {
    const res = await apiFetch('/api/hosts/check-all', { method: 'POST' });
    if (!res) return;
    const data = await res.json();
    if (data.ok && Array.isArray(data.hosts)) {
      hosts.value = data.hosts;
      seedInspectionMap(data.inspections);
      const online = data.hosts.filter((h: PublicHost) => !h.archived && h.status === 'online').length;
      showToast(`机队快照同步完成：${online} 台主机在线`);
    }
  } catch (e) {
    showToast(`批量检测失败: ${(e as Error).message}`, 'err');
  } finally {
    checkingAll.value = false;
  }
}

function syncCurrentTabState() {
  if (!stageHostId.value) return;
  const tab = openTabs.value.find((t) => t.hostId === stageHostId.value);
  if (tab && centerStage.value) {
    tab.mode = centerStage.value;
    tab.rightWing = rightWing.value;
  }
}

function switchToHomeView() {
  activePage.value = 'workspace';
  syncCurrentTabState();
  centerStage.value = null;
  stageHostId.value = null;
  if (rightWing.value === 'files') {
    rightWing.value = null;
  }
}

async function ensureHostHasPane(tab: OpenSessionTab) {
  if (tab.panes.length > 0) return;
  await createPaneForTab(tab);
}

async function createPaneForTab(tab: OpenSessionTab, cwd?: string | null) {
  // 按主机去重：全局布尔会让"主机 A 创建中"静默吞掉"主机 B 的创建请求"，
  // 导致 B 的标签页永久停在"正在连接"。
  if (paneCreationInFlight.has(tab.hostId)) return;
  paneCreationInFlight.add(tab.hostId);
  paneCreationCount.value += 1;
  try {
    const res = await apiFetch('/api/terminals', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        hostId: tab.hostId,
        cols: 100,
        rows: 30,
        cwd: cwd || undefined,
      }),
    });
    if (!res) return;
    const data = await res.json();
    if (!data.ok || !data.pane) {
      // 建不出来就把原因留在标签上，交给舞台渲染成"连接失败 + 重试"。
      // 旧行为只弹一次 toast 就 return，panes 永远是空，舞台就一直转"正在连接"——
      // 主机离线、密码错、会话过期都会撞上，独立窗口模式下连标签栏都没有，无路可退。
      tab.error = (data.error as string) || '启动终端失败';
      showToast(tab.error, 'err');
      void fetchHosts(true);
      return;
    }
    tab.error = null;
    tab.panes.push(data.pane as PaneMeta);
    const host = hostById(tab.hostId);
    if (host && host.status !== 'online') {
      host.status = 'online';
    }
    void fetchHosts(true);
  } catch (e) {
    tab.error = `启动终端失败: ${(e as Error).message}`;
    showToast(tab.error, 'err');
  } finally {
    paneCreationInFlight.delete(tab.hostId);
    paneCreationCount.value = Math.max(0, paneCreationCount.value - 1);
  }
}

/// 舞台上的「重试」按钮：清掉失败态后重新建 pane
async function retryPaneForTab(tab: OpenSessionTab) {
  tab.error = null;
  await createPaneForTab(tab);
}

async function activateSessionTab(tab: OpenSessionTab) {
  activePage.value = 'workspace';
  syncCurrentTabState();
  stageHostId.value = tab.hostId;
  centerStage.value = tab.mode;
  detailHostId.value = tab.hostId;
  rightWing.value = tab.rightWing;
  if (tab.mode === 'terminal') {
    await ensureHostHasPane(tab);
  }
}

async function closeSessionTab(hostId: string) {
  const idx = openTabs.value.findIndex((t) => t.hostId === hostId);
  if (idx >= 0) {
    const [removed] = openTabs.value.splice(idx, 1);
    for (const p of removed.panes) {
      void fetch(`/api/terminals/${encodeURIComponent(p.id)}`, { method: 'DELETE' });
    }
  }
  if (stageHostId.value === hostId) {
    const fallback = openTabs.value[openTabs.value.length - 1];
    if (fallback) {
      await activateSessionTab(fallback);
    } else {
      switchToHomeView();
    }
  }
}

async function handlePopoutTerminal(hostId: string) {
  try {
    const res = await apiFetch('/api/window/open-terminal', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ hostId }),
    });
    if (!res) return;
    const data = await res.json();
    if (data.ok) {
      const h = hostById(hostId);
      showToast(`已在独立原生窗口打开「${h?.name || hostId}」终端`);
    } else {
      showToast(data.error || '打开独立窗口失败', 'err');
    }
  } catch (e) {
    showToast(`打开独立窗口失败: ${(e as Error).message}`, 'err');
  }
}

function prewarmHostFiles(host: PublicHost) {
  if (host.status === 'offline') return;
  void fetch('/api/files/prewarm', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ hostId: host.id }),
  });
}

function onCardClick(host: PublicHost) {
  if (rightWing.value === 'detail' && detailHostId.value === host.id) {
    rightWing.value = null;
    detailHostId.value = null;
  } else {
    detailHostId.value = host.id;
    rightWing.value = 'detail';
    prewarmHostFiles(host);
  }
  syncCurrentTabState();
}

async function openTerminalStage(host: PublicHost) {
  activePage.value = 'workspace';
  prewarmHostFiles(host);
  syncCurrentTabState();
  let tab = openTabs.value.find((t) => t.hostId === host.id);
  if (!tab) {
    tab = {
      hostId: host.id,
      mode: 'terminal',
      rightWing: rightWing.value === 'detail' && detailHostId.value === host.id ? 'detail' : null,
      splitDirection: 'row',
      panes: [],
    };
    openTabs.value.push(tab);
  } else {
    tab.mode = 'terminal';
  }
  stageHostId.value = host.id;
  centerStage.value = 'terminal';
  detailHostId.value = host.id;
  rightWing.value = tab.rightWing;
  await ensureHostHasPane(tab);
}

function openFilesStage(host: PublicHost) {
  activePage.value = 'workspace';
  syncCurrentTabState();
  let tab = openTabs.value.find((t) => t.hostId === host.id);
  if (!tab) {
    tab = {
      hostId: host.id,
      mode: 'files',
      rightWing: null,
      splitDirection: 'row',
      panes: [],
    };
    openTabs.value.push(tab);
  } else {
    tab.mode = 'files';
    if (tab.rightWing === 'files') tab.rightWing = null;
  }
  stageHostId.value = host.id;
  centerStage.value = 'files';
  detailHostId.value = host.id;
  if (rightWing.value === 'files') {
    rightWing.value = null;
  }
}

function toggleRightWing(mode: 'files' | 'detail') {
  if (!stageHost.value) return;
  if (rightWing.value === mode && detailHostId.value === stageHost.value.id) {
    rightWing.value = null;
  } else {
    detailHostId.value = stageHost.value.id;
    rightWing.value = mode;
  }
  syncCurrentTabState();
}

async function handleSplitTerminal(direction: 'row' | 'col') {
  const tab = activeSessionTab.value;
  if (!tab) return;
  tab.splitDirection = direction;
  const lastCwd = tab.panes[tab.panes.length - 1]?.cwd;
  await createPaneForTab(tab, lastCwd);
}

async function handleClosePane(paneId: string) {
  const tab = activeSessionTab.value;
  if (!tab) return;
  void fetch(`/api/terminals/${encodeURIComponent(paneId)}`, { method: 'DELETE' });
  tab.panes = tab.panes.filter((p) => p.id !== paneId);
  if (tab.panes.length === 0) {
    await closeSessionTab(tab.hostId);
  }
}

// 终端刷新 / 会话结束后原地重连：先 DELETE 旧 pane（若该主机仅此一个 pane，后端 close_pane
// 会立即调用 drop_pane_session 释放可能半死不活的旧 SSH 连接），再新建全新 pane 顶替原位置。
async function handleReconnectPane(paneId: string) {
  const tab = openTabs.value.find((t) => t.panes.some((p) => p.id === paneId));
  if (!tab || paneCreationInFlight.has(tab.hostId)) return;
  const idx = tab.panes.findIndex((p) => p.id === paneId);
  const cwd = tab.panes[idx]?.cwd || undefined;
  tab.error = null;
  paneCreationInFlight.add(tab.hostId);
  paneCreationCount.value += 1;
  try {
    await fetch(`/api/terminals/${encodeURIComponent(paneId)}`, { method: 'DELETE' }).catch(() => {});
    const res = await apiFetch('/api/terminals', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ hostId: tab.hostId, cols: 100, rows: 30, cwd: cwd || undefined }),
    });
    if (!res) return;
    const data = await res.json();
    if (!data.ok || !data.pane) {
      const errMsg = data.error || '刷新终端失败';
      if (tab.panes.length <= 1) {
        tab.panes = [];
        tab.error = errMsg;
      }
      showToast(errMsg, 'err');
      return;
    }
    const at = tab.panes.findIndex((p) => p.id === paneId);
    if (at >= 0) {
      tab.panes.splice(at, 1, data.pane as PaneMeta);
    } else {
      tab.panes.push(data.pane as PaneMeta);
    }
    showToast('终端已刷新重连');
  } catch (e) {
    const errMsg = `刷新终端失败: ${(e as Error).message}`;
    if (tab.panes.length <= 1) {
      tab.panes = [];
      tab.error = errMsg;
    }
    showToast(errMsg, 'err');
  } finally {
    paneCreationInFlight.delete(tab.hostId);
    paneCreationCount.value = Math.max(0, paneCreationCount.value - 1);
  }
}

function handlePaneMetaUpdate(updated: PaneMeta) {
  for (const tab of openTabs.value) {
    const idx = tab.panes.findIndex((p) => p.id === updated.id);
    if (idx >= 0) {
      tab.panes.splice(idx, 1, updated);
      break;
    }
  }
}

function openCreateModal() {
  editingHost.value = null;
  modalOpen.value = true;
}

function openEditModal(host: PublicHost) {
  editingHost.value = host;
  modalOpen.value = true;
}

async function handleModalSubmit(payload: HostUpsertPayload, editingId: string | null) {
  try {
    const url = editingId ? `/api/hosts/${encodeURIComponent(editingId)}` : '/api/hosts';
    const method = editingId ? 'PUT' : 'POST';
    const res = await apiFetch(url, {
      method,
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    });
    if (!res) return;
    const data = await res.json();
    if (!data.ok) {
      showToast(data.error || '保存失败', 'err');
      return;
    }
    modalOpen.value = false;
    await fetchHosts();
    if (data.host?.id) {
      detailHostId.value = data.host.id;
      rightWing.value = 'detail';
      void checkSingleHost(data.host as PublicHost);
    }
    showToast(editingId ? '主机已更新，正在检测连接…' : '主机已创建，正在检测连接…');
  } catch (e) {
    showToast(`保存失败: ${(e as Error).message}`, 'err');
  }
}

async function handlePatchMeta(host: PublicHost, patch: Record<string, unknown>) {
  if (typeof patch.pinned === 'boolean') {
    host.pinned = patch.pinned;
    sortHostsInPlace();
  }
  try {
    const res = await apiFetch(`/api/hosts/${encodeURIComponent(host.id)}/meta`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(patch),
    });
    if (!res) return;
    const data = await res.json();
    if (!data.ok || !data.host) {
      showToast(data.error || '更新失败', 'err');
      await fetchHosts();
      return;
    }
    if (Array.isArray(data.hosts)) {
      hosts.value = data.hosts;
    } else {
      replaceHostInList(data.host);
    }
  } catch (e) {
    showToast(`更新失败: ${(e as Error).message}`, 'err');
  }
}

const activeUnarchivedHosts = computed(() => hosts.value.filter((h) => !h.archived));
const mcpExposedCount = computed(
  () => activeUnarchivedHosts.value.filter((h) => h.mcpExposed !== false).length
);

function toggleHostMcpExposed(host: PublicHost) {
  const next = host.mcpExposed === false;
  host.mcpExposed = next;
  void handlePatchMeta(host, { mcpExposed: next });
}

async function setAllHostsMcpExposed(exposed: boolean) {
  try {
    const res = await apiFetch('/api/hosts/mcp-expose', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ exposed }),
    });
    if (!res) return;
    const data = await res.json();
    if (!data.ok || !Array.isArray(data.hosts)) {
      showToast(data.error || '批量更新 MCP 暴露范围失败', 'err');
      await fetchHosts();
      return;
    }
    hosts.value = data.hosts;
    showToast(exposed ? '已将全部主机设为对 MCP 暴露' : '已隐藏全部主机的 MCP 暴露');
  } catch (e) {
    showToast(`操作失败: ${(e as Error).message}`, 'err');
  }
}

async function saveMcpTools(nextTools: string[]) {
  mcpEnabledTools.value = nextTools;
  try {
    const res = await apiFetch('/api/settings/mcp', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ mcpEnabledTools: nextTools }),
    });
    if (!res) return;
    const data = await res.json();
    if (!data.ok || !data.settings) {
      showToast(data.error || '保存 MCP 工具开放范围失败', 'err');
      await loadSettings();
      return;
    }
    if (Array.isArray(data.settings.mcpEnabledTools)) {
      mcpEnabledTools.value = data.settings.mcpEnabledTools;
    }
  } catch (e) {
    showToast(`保存失败: ${(e as Error).message}`, 'err');
  }
}

function toggleMcpTool(toolName: string) {
  const exists = mcpEnabledTools.value.includes(toolName);
  const next = exists
    ? mcpEnabledTools.value.filter((t) => t !== toolName)
    : [...mcpEnabledTools.value, toolName];
  void saveMcpTools(next);
}

function setAllMcpTools(enabled: boolean) {
  const next = enabled ? mcpToolCatalog.map((t) => t.name) : [];
  void saveMcpTools(next);
}

function handleDeleteHost(host: PublicHost) {
  if (host.kind === 'local') return;
  deletingHost.value = host;
}

async function confirmDeleteHost() {
  const host = deletingHost.value;
  if (!host) return;
  deletingHost.value = null;
  try {
    const res = await apiFetch(`/api/hosts/${encodeURIComponent(host.id)}`, {
      method: 'DELETE',
    });
    if (!res) return;
    const data = await res.json();
    if (!data.ok) {
      // 删不掉就得说，不能静默关掉对话框让人以为删了
      showToast(data.error || `删除主机「${host.name}」失败`, 'err');
      await fetchHosts();
      return;
    }
    await closeSessionTab(host.id);
    if (detailHostId.value === host.id) {
      detailHostId.value = null;
      rightWing.value = null;
    }
    await fetchHosts();
    showToast(`已删除主机「${host.name}」`);
  } catch (e) {
    showToast(`删除失败: ${(e as Error).message}`, 'err');
    await fetchHosts();
  }
}

function handleGlobalKeydown(e: KeyboardEvent) {
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k') {
    e.preventDefault();
    if (activePage.value !== 'workspace') activePage.value = 'workspace';
    searchInputRef.value?.focus();
    searchInputRef.value?.select();
  } else if (e.key === 'Escape') {
    if (filterMenuOpen.value) filterMenuOpen.value = false;
    else if (deletingHost.value) deletingHost.value = null;
    else if (modalOpen.value) modalOpen.value = false;
  }
}

function onDragStart(e: DragEvent, host: PublicHost) {
  if (host.kind === 'local') return;
  draggingId.value = host.id;
  e.dataTransfer?.setData('text/plain', host.id);
  if (e.dataTransfer) e.dataTransfer.effectAllowed = 'move';
}

function onDragOver(e: DragEvent, target: PublicHost) {
  if (!draggingId.value || draggingId.value === target.id || target.kind === 'local') return;
  e.preventDefault();
  dragOverId.value = target.id;
}

async function onDrop(e: DragEvent, target: PublicHost) {
  e.preventDefault();
  const fromId = draggingId.value;
  draggingId.value = null;
  dragOverId.value = null;
  if (!fromId || fromId === target.id || target.kind === 'local') return;

  const currentIds = hosts.value.filter((h) => h.kind !== 'local').map((h) => h.id);
  const fromIdx = currentIds.indexOf(fromId);
  const toIdx = currentIds.indexOf(target.id);
  if (fromIdx < 0 || toIdx < 0) return;

  currentIds.splice(fromIdx, 1);
  currentIds.splice(toIdx, 0, fromId);

  try {
    const res = await apiFetch('/api/hosts/reorder', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ orderedIds: currentIds }),
    });
    if (!res) return;
    const data = await res.json();
    if (!data.ok || !Array.isArray(data.hosts)) {
      showToast(data.error || '排序未保存', 'err');
      await fetchHosts();
      return;
    }
    hosts.value = data.hosts;
  } catch (e) {
    showToast(`排序未保存: ${(e as Error).message}`, 'err');
    await fetchHosts();
  }
}

function onDragEnd() {
  draggingId.value = null;
  dragOverId.value = null;
}

async function initAuthenticatedWorkspace() {
  await fetchHosts();
  await restoreExistingTerminals();
  await loadSettings();
  subscribeUiEvents();

  if (isStandalone.value) {
    const target = hosts.value.find((h) => h.id === standaloneHostId.value) || hosts.value[0];
    if (target) {
      stageHostId.value = target.id;
      centerStage.value = 'terminal';
      detailHostId.value = target.id;
      let tab = openTabs.value.find((t) => t.hostId === target.id);
      if (!tab) {
        tab = {
          hostId: target.id,
          mode: 'terminal',
          rightWing: null,
          splitDirection: 'row',
          panes: [],
        };
        openTabs.value.push(tab);
      }
      await ensureHostHasPane(tab);
    }
  }
}

onMounted(async () => {
  window.addEventListener('keydown', handleGlobalKeydown);
  try {
    const saved = localStorage.getItem('vps-switch-theme');
    if (saved === 'dark') applyTheme(true);
  } catch {
    // ignore
  }
  await checkAuthStatus();
  if (authenticated.value) {
    await initAuthenticatedWorkspace();
  }
});

onBeforeUnmount(() => {
  window.removeEventListener('keydown', handleGlobalKeydown);
  uiEventSource?.close();
});
</script>

<template>
  <!-- 1. 当启用了登录保护且未登录时：展示独立毛玻璃登录界面 -->
  <div
    v-if="authChecked && authEnabled && !authenticated"
    class="flex h-full w-full items-center justify-center p-4 select-none text-zinc-900 dark:text-zinc-100"
  >
    <div
      class="w-full max-w-sm rounded-3xl border border-zinc-200/85 bg-white/85 p-7 shadow-[0_8px_36px_rgba(0,0,0,0.06)] backdrop-blur-xl dark:border-zinc-800 dark:bg-zinc-900/88"
    >
      <div class="flex flex-col items-center text-center">
        <BrandLogo size="lg" />
        <h1 class="mt-3 text-base font-bold tracking-tight">VPS Switch</h1>
        <p class="mt-1 text-xs text-zinc-400">请输入管理员账号与密码登录</p>
      </div>

      <form class="mt-6 space-y-3.5" @submit.prevent="handleLoginSubmit">
        <div
          v-if="loginError"
          class="rounded-xl border border-red-200 bg-red-50 px-3 py-2 text-xs text-red-600 dark:border-red-900/60 dark:bg-red-950/40 dark:text-red-400"
        >
          {{ loginError }}
        </div>

        <div>
          <label class="block text-xs font-medium text-zinc-600 dark:text-zinc-300">管理员账号</label>
          <input
            v-model="loginUsername"
            type="text"
            autocomplete="username"
            class="mt-1 w-full rounded-xl border border-zinc-200 bg-zinc-50/70 px-3 py-2 text-xs outline-none transition focus:border-zinc-400 focus:bg-white dark:border-zinc-700 dark:bg-zinc-800 dark:focus:border-zinc-500"
          />
        </div>

        <div>
          <label class="block text-xs font-medium text-zinc-600 dark:text-zinc-300">密码</label>
          <input
            v-model="loginPassword"
            type="password"
            autocomplete="current-password"
            placeholder="输入登录密码"
            autofocus
            class="mt-1 w-full rounded-xl border border-zinc-200 bg-zinc-50/70 px-3 py-2 text-xs outline-none transition focus:border-zinc-400 focus:bg-white dark:border-zinc-700 dark:bg-zinc-800 dark:focus:border-zinc-500"
          />
        </div>

        <button
          type="submit"
          :disabled="loginSubmitting"
          class="mt-2 w-full rounded-xl bg-zinc-900 py-2.5 text-xs font-semibold text-white shadow-sm transition hover:bg-zinc-800 disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-white"
        >
          {{ loginSubmitting ? '正在验证...' : '登录' }}
        </button>
      </form>
    </div>
  </div>

  <!-- 2. 独立系统原生终端窗口（100% 独占沉浸纯终端，无侧栏与管理后台杂质） -->
  <div
    v-else-if="authChecked && isStandalone"
    class="flex h-full w-full flex-col overflow-hidden select-none bg-white dark:bg-[#0c0d10] text-zinc-900 dark:text-zinc-100"
  >
    <div
      v-for="tab in openTabs"
      :key="tab.hostId"
      v-show="stageHostId === tab.hostId"
      class="h-full w-full"
    >
      <TerminalStage
        v-if="hostById(tab.hostId)"
        :host="hostById(tab.hostId)!"
        :panes="tab.panes"
        :split-direction="tab.splitDirection"
        :dark="terminalDark"
        :right-wing="null"
        :creating="creatingPane"
        :standalone="true"
        :error="tab.error"
        @split="handleSplitTerminal"
        @close-pane="handleClosePane"
        @reconnect-pane="handleReconnectPane"
        @retry="retryPaneForTab(tab)"
        @toggle-dark="toggleTheme"
        @meta-update="handlePaneMetaUpdate"
      />
    </div>
  </div>

  <!-- 3. 已认证常规主应用（仓库 + 工作台 + 设置） -->
  <div
    v-else-if="authChecked"
    class="flex h-full w-full flex-col overflow-hidden select-none text-zinc-900 dark:text-zinc-100"
    @click="filterMenuOpen = false"
  >
    <!-- 顶部栏 -->
    <header
      class="relative z-30 flex h-13 shrink-0 items-center justify-between gap-3 border-b border-zinc-200/75 bg-white/75 px-4 py-2 backdrop-blur-xl dark:border-zinc-800/80 dark:bg-zinc-900/75"
    >
      <!-- 左上角：品牌 + ⌂ 仓库按钮 + 已打开的多终端会话标签页 -->
      <div class="flex min-w-0 flex-1 items-center gap-1.5 overflow-x-auto pr-2">
        <div class="mr-1 flex shrink-0 items-center gap-2 pr-1">
          <BrandLogo size="sm" />
          <span class="text-sm font-bold tracking-tight text-zinc-900 dark:text-zinc-100">VPS Switch</span>
        </div>

        <!-- ⌂ 仓库 (Home) 按钮 -->
        <button
          type="button"
          class="inline-flex shrink-0 items-center gap-1.5 rounded-xl px-3 py-1.5 text-xs font-medium transition"
          :class="
            activePage === 'workspace' && !hasCenterStage
              ? 'border border-zinc-200/90 bg-white text-zinc-900 shadow-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-100'
              : 'text-zinc-500 hover:bg-white/60 hover:text-zinc-900 dark:text-zinc-400 dark:hover:bg-zinc-800/60 dark:hover:text-zinc-100'
          "
          title="回到全宽仓库视图"
          @click="switchToHomeView"
        >
          <svg class="h-3.5 w-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M3 10.5L12 3l9 7.5V20a1 1 0 0 1-1 1h-5.5v-6h-5v6H4a1 1 0 0 1-1-1v-9.5z" />
          </svg>
          <span>仓库</span>
          <span class="rounded-full bg-zinc-100 px-1.5 py-0.2 font-mono text-[10px] text-zinc-500 dark:bg-zinc-700 dark:text-zinc-300">
            {{ onlineCount }}/{{ totalActiveCount }}
          </span>
        </button>

        <!-- 侧栏显隐切换按钮（仅在打开终端或文件时展示，默认收起让终端独占全宽） -->
        <button
          v-if="activePage === 'workspace' && hasCenterStage"
          type="button"
          class="inline-flex shrink-0 items-center gap-1.5 rounded-xl px-2.5 py-1.5 text-xs font-medium transition"
          :class="
            showSidebarInStage
              ? 'border border-zinc-200/90 bg-white text-zinc-900 shadow-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-100'
              : 'text-zinc-500 hover:bg-white/60 hover:text-zinc-900 dark:text-zinc-400 dark:hover:bg-zinc-800/60 dark:hover:text-zinc-100'
          "
          :title="showSidebarInStage ? '收起侧边主机列表，终端全宽显示' : '展开侧边主机列表并排查看'"
          @click="showSidebarInStage = !showSidebarInStage"
        >
          <svg class="h-3.5 w-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <rect x="3" y="3" width="18" height="18" rx="2" ry="2" />
            <line x1="9" y1="3" x2="9" y2="21" />
          </svg>
          <span class="hidden sm:inline">{{ showSidebarInStage ? '收起侧栏' : '展开侧栏' }}</span>
        </button>

        <!-- 左上角已打开的终端/文件会话标签页 -->
        <div
          v-for="tab in openTabs"
          :key="tab.hostId"
          class="group inline-flex shrink-0 items-center gap-2 rounded-xl px-3 py-1.5 text-xs font-medium transition cursor-pointer"
          :class="
            activePage === 'workspace' && hasCenterStage && stageHostId === tab.hostId
              ? 'border border-zinc-200/90 bg-white text-zinc-900 shadow-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-100'
              : 'border border-transparent bg-zinc-100/60 text-zinc-600 hover:bg-white/80 hover:text-zinc-900 dark:bg-zinc-800/40 dark:text-zinc-400 dark:hover:bg-zinc-800 dark:hover:text-zinc-100'
          "
          @click="activateSessionTab(tab)"
        >
          <span
            class="h-2 w-2 shrink-0 rounded-full"
            :class="{
              'bg-emerald-500': hostById(tab.hostId)?.status === 'online',
              'bg-red-500': hostById(tab.hostId)?.status === 'offline',
              'bg-zinc-300 dark:bg-zinc-600': !hostById(tab.hostId) || hostById(tab.hostId)?.status === 'unknown',
            }"
          />
          <span class="max-w-[128px] truncate">{{ hostById(tab.hostId)?.name || tab.hostId }}</span>
          <span
            v-if="tab.panes.length > 1"
            class="rounded bg-zinc-100 px-1 py-0.2 font-mono text-[10px] text-zinc-500 dark:bg-zinc-700 dark:text-zinc-300"
          >
            ×{{ tab.panes.length }}
          </span>
          <button
            type="button"
            class="rounded p-0.5 text-zinc-400 transition hover:bg-zinc-200/70 hover:text-zinc-800 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
            title="关闭该会话"
            @click.stop="closeSessionTab(tab.hostId)"
          >
            ×
          </button>
        </div>
      </div>

      <!-- 右上角 -->
      <div class="flex shrink-0 items-center gap-2">
        <template v-if="activePage === 'workspace'">
          <!-- 搜索框 -->
          <div class="relative hidden sm:block">
            <svg
              class="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-zinc-400"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
            >
              <circle cx="11" cy="11" r="7" />
              <path d="M20 20l-3.5-3.5" />
            </svg>
            <input
              ref="searchInputRef"
              v-model="searchQuery"
              type="text"
              placeholder="搜索名称 / IP / 标签 (Ctrl+K)"
              class="w-44 rounded-full border border-zinc-200/80 bg-white/85 py-1.5 pl-8 pr-3 text-xs text-zinc-800 outline-none transition focus:w-56 focus:border-zinc-300 focus:bg-white dark:border-zinc-700/80 dark:bg-zinc-800/85 dark:text-zinc-100 dark:focus:border-zinc-600 dark:focus:bg-zinc-800"
            />
          </div>

          <!-- 统一筛选按钮 -->
          <div class="relative" @click.stop>
            <button
              type="button"
              class="inline-flex items-center gap-1.5 rounded-full border px-3 py-1.5 text-xs font-medium transition"
              :class="
                hasActiveFilter
                  ? 'border-zinc-900 bg-zinc-900 text-white shadow-sm dark:border-zinc-100 dark:bg-zinc-100 dark:text-zinc-900'
                  : 'border-zinc-200/80 bg-white/85 text-zinc-700 hover:bg-white dark:border-zinc-700/80 dark:bg-zinc-800/85 dark:text-zinc-200 dark:hover:bg-zinc-800'
              "
              @click="filterMenuOpen = !filterMenuOpen"
            >
              <svg class="h-3.5 w-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <polygon points="22 3 2 3 10 12.46 10 19 14 21 14 12.46 22 3" />
              </svg>
              <span>{{ activeFilterSummary }}</span>
              <span
                v-if="hasActiveFilter"
                class="ml-0.5 rounded-full px-1 hover:bg-white/20 dark:hover:bg-black/20"
                title="重置筛选"
                @click.stop="resetFilters"
              >
                ×
              </span>
            </button>

            <div
              v-if="filterMenuOpen"
              class="absolute right-0 top-full mt-2 w-64 rounded-2xl border border-zinc-200/90 bg-white/95 p-3.5 shadow-xl backdrop-blur-xl dark:border-zinc-700/90 dark:bg-zinc-900/95"
            >
              <div>
                <div class="flex items-center justify-between text-[11px] font-semibold text-zinc-400">
                  <span>在线状态</span>
                  <button
                    v-if="hasActiveFilter"
                    type="button"
                    class="text-[11px] font-normal text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-200"
                    @click="resetFilters"
                  >
                    重置全部
                  </button>
                </div>
                <div class="mt-1.5 grid grid-cols-3 gap-1">
                  <button
                    type="button"
                    class="rounded-xl px-2 py-1.5 text-xs font-medium transition"
                    :class="
                      statusFilter === 'all'
                        ? 'bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900'
                        : 'bg-zinc-100 text-zinc-600 hover:bg-zinc-200/70 dark:bg-zinc-800 dark:text-zinc-300'
                    "
                    @click="statusFilter = 'all'"
                  >
                    全部
                  </button>
                  <button
                    type="button"
                    class="inline-flex items-center justify-center gap-1 rounded-xl px-2 py-1.5 text-xs font-medium transition"
                    :class="
                      statusFilter === 'online'
                        ? 'bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900'
                        : 'bg-zinc-100 text-zinc-600 hover:bg-zinc-200/70 dark:bg-zinc-800 dark:text-zinc-300'
                    "
                    @click="statusFilter = 'online'"
                  >
                    <span class="h-2 w-2 rounded-full bg-emerald-500" />
                    <span>在线 ({{ onlineCount }})</span>
                  </button>
                  <button
                    type="button"
                    class="inline-flex items-center justify-center gap-1 rounded-xl px-2 py-1.5 text-xs font-medium transition"
                    :class="
                      statusFilter === 'offline'
                        ? 'bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900'
                        : 'bg-zinc-100 text-zinc-600 hover:bg-zinc-200/70 dark:bg-zinc-800 dark:text-zinc-300'
                    "
                    @click="statusFilter = 'offline'"
                  >
                    <span class="h-2 w-2 rounded-full bg-red-500" />
                    <span>离线 ({{ offlineCount }})</span>
                  </button>
                </div>
              </div>

              <div class="mt-3 border-t border-zinc-100 pt-3 dark:border-zinc-800">
                <div class="text-[11px] font-semibold text-zinc-400">收藏与分类</div>
                <div class="mt-1.5 flex flex-wrap gap-1">
                  <button
                    type="button"
                    class="inline-flex items-center gap-1 rounded-xl px-2.5 py-1 text-xs font-medium transition"
                    :class="
                      favoriteOnly
                        ? 'bg-amber-500 text-white shadow-sm'
                        : 'bg-zinc-100 text-zinc-600 hover:bg-zinc-200/70 dark:bg-zinc-800 dark:text-zinc-300'
                    "
                    @click="favoriteOnly = !favoriteOnly"
                  >
                    <span>★ 收藏 ({{ favoriteCount }})</span>
                  </button>
                  <button
                    v-for="opt in roleFilterOptions"
                    :key="opt.key"
                    type="button"
                    class="rounded-xl px-2.5 py-1 text-xs font-medium transition"
                    :class="
                      roleFilter === opt.key
                        ? 'bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900'
                        : 'bg-zinc-100 text-zinc-600 hover:bg-zinc-200/70 dark:bg-zinc-800 dark:text-zinc-300'
                    "
                    @click="roleFilter = opt.key"
                  >
                    {{ opt.label }}
                  </button>
                </div>
              </div>

              <div v-if="allTags.length" class="mt-3 border-t border-zinc-100 pt-3 dark:border-zinc-800">
                <div class="text-[11px] font-semibold text-zinc-400">标签</div>
                <div class="mt-1.5 flex max-h-28 flex-wrap gap-1 overflow-y-auto">
                  <button
                    v-for="t in allTags"
                    :key="t"
                    type="button"
                    class="rounded-lg px-2 py-0.5 text-[11px] font-medium transition"
                    :class="
                      tagFilter === t
                        ? 'bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900'
                        : 'bg-zinc-100 text-zinc-600 hover:bg-zinc-200/70 dark:bg-zinc-800 dark:text-zinc-300'
                    "
                    @click="tagFilter = tagFilter === t ? '' : t"
                  >
                    #{{ t }}
                  </button>
                </div>
              </div>
            </div>
          </div>

          <!-- 检测在线按钮 -->
          <button
            type="button"
            class="inline-flex items-center gap-1 rounded-full border border-zinc-200/80 bg-white/85 px-3 py-1.5 text-xs font-medium text-zinc-700 transition hover:bg-white disabled:opacity-50 dark:border-zinc-700/80 dark:bg-zinc-800/85 dark:text-zinc-200 dark:hover:bg-zinc-800"
            :disabled="checkingAll"
            title="并发检测全部 VPS 在线状态并刷新硬件快照"
            @click="checkAllHosts"
          >
            <svg
              class="h-3.5 w-3.5"
              :class="{ 'animate-spin': checkingAll }"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
            >
              <path d="M21 12a9 9 0 1 1-3-6.7" />
              <path d="M21 3v6h-6" />
            </svg>
            <span>{{ checkingAll ? '检测中' : '检测在线' }}</span>
          </button>
        </template>

        <!-- 昼夜主题切换按钮 -->
        <button
          type="button"
          class="flex h-8 w-8 items-center justify-center rounded-full border border-zinc-200/80 bg-white/85 text-zinc-600 transition hover:bg-white hover:text-zinc-900 dark:border-zinc-700/80 dark:bg-zinc-800/85 dark:text-zinc-300 dark:hover:bg-zinc-800 dark:hover:text-white"
          :title="isDark ? '切换为日间白色毛玻璃' : '切换为夜间深色毛玻璃'"
          @click="toggleTheme"
        >
          <svg v-if="isDark" class="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <circle cx="12" cy="12" r="4" />
            <path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M4.93 19.07l1.41-1.41M17.66 6.34l1.41-1.41" />
          </svg>
          <svg v-else class="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" />
          </svg>
        </button>

        <!-- 独立设置页入口按钮 -->
        <button
          type="button"
          class="flex h-8 w-8 items-center justify-center rounded-full border transition"
          :class="
            activePage === 'settings'
              ? 'border-zinc-900 bg-zinc-900 text-white shadow-sm dark:border-zinc-100 dark:bg-zinc-100 dark:text-zinc-900'
              : 'border-zinc-200/80 bg-white/85 text-zinc-600 hover:bg-white hover:text-zinc-900 dark:border-zinc-700/80 dark:bg-zinc-800/85 dark:text-zinc-300 dark:hover:bg-zinc-800 dark:hover:text-white'
          "
          title="进入设置页（MCP Server / 调用记录 / 登录安全）"
          @click="openSettingsPage()"
        >
          <svg class="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <circle cx="12" cy="12" r="3" />
            <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
          </svg>
        </button>

        <!-- 新增 VPS 橙色加号 -->
        <button
          type="button"
          class="flex h-8 w-8 items-center justify-center rounded-full bg-orange-500 text-white shadow-sm transition hover:bg-orange-600 active:scale-95"
          title="新增 VPS"
          @click="openCreateModal"
        >
          <svg class="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5">
            <path d="M12 5v14M5 12h14" />
          </svg>
        </button>
      </div>
    </header>

    <!-- 页面 A：独立设置页面（非弹窗，直接全页展示 MCP 开关、API Key、登录账号密码与外观） -->
    <main
      v-if="activePage === 'settings'"
      class="flex-1 overflow-y-auto p-5"
    >
      <div class="mx-auto max-w-4xl space-y-4">
        <!-- 顶部返回条 -->
        <div class="flex items-center justify-between">
          <div>
            <h2 class="text-base font-bold">系统设置</h2>
            <p class="text-xs text-zinc-400">管理 MCP Server 远程/本机接入、调用记录与 Web 访问安全</p>
          </div>
          <div class="flex items-center gap-2">
            <button
              v-if="authEnabled"
              type="button"
              class="rounded-xl border border-red-200/80 bg-white px-3 py-1.5 text-xs font-medium text-red-600 hover:bg-red-50 dark:border-red-900/50 dark:bg-zinc-900 dark:text-red-400"
              @click="handleLogout"
            >
              退出登录
            </button>
            <button
              type="button"
              class="rounded-xl border border-zinc-200/80 bg-white px-3.5 py-1.5 text-xs font-medium text-zinc-700 hover:bg-zinc-50 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
              @click="activePage = 'workspace'"
            >
              返回仓库
            </button>
          </div>
        </div>

        <!-- 分类切换导航条（毛玻璃胶囊 Tab） -->
        <div class="flex items-center gap-1.5 overflow-x-auto rounded-2xl border border-zinc-200/80 bg-white/75 p-1.5 backdrop-blur-xl dark:border-zinc-800 dark:bg-zinc-900/75">
          <button
            type="button"
            class="flex items-center gap-2 rounded-xl px-4 py-2 text-xs font-medium transition cursor-pointer"
            :class="
              activeSettingsTab === 'general'
                ? 'bg-zinc-900 text-white shadow-sm dark:bg-zinc-100 dark:text-zinc-900'
                : 'text-zinc-600 hover:bg-zinc-100/80 dark:text-zinc-400 dark:hover:bg-zinc-800'
            "
            @click="activeSettingsTab = 'general'"
          >
            <svg class="h-3.5 w-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <circle cx="12" cy="12" r="3" />
              <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
            </svg>
            <span>常规与自启</span>
          </button>

          <button
            type="button"
            class="flex items-center gap-2 rounded-xl px-4 py-2 text-xs font-medium transition cursor-pointer"
            :class="
              activeSettingsTab === 'security'
                ? 'bg-zinc-900 text-white shadow-sm dark:bg-zinc-100 dark:text-zinc-900'
                : 'text-zinc-600 hover:bg-zinc-100/80 dark:text-zinc-400 dark:hover:bg-zinc-800'
            "
            @click="activeSettingsTab = 'security'"
          >
            <svg class="h-3.5 w-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <rect x="3" y="11" width="18" height="11" rx="2" ry="2" />
              <path d="M7 11V7a5 5 0 0 1 10 0v4" />
            </svg>
            <span>访问安全</span>
          </button>

          <button
            type="button"
            class="flex items-center gap-2 rounded-xl px-4 py-2 text-xs font-medium transition cursor-pointer"
            :class="
              activeSettingsTab === 'mcp'
                ? 'bg-zinc-900 text-white shadow-sm dark:bg-zinc-100 dark:text-zinc-900'
                : 'text-zinc-600 hover:bg-zinc-100/80 dark:text-zinc-400 dark:hover:bg-zinc-800'
            "
            @click="activeSettingsTab = 'mcp'"
          >
            <svg class="h-3.5 w-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2" />
            </svg>
            <span>MCP Server</span>
          </button>

          <button
            type="button"
            class="flex items-center gap-2 rounded-xl px-4 py-2 text-xs font-medium transition cursor-pointer"
            :class="
              activeSettingsTab === 'calls'
                ? 'bg-zinc-900 text-white shadow-sm dark:bg-zinc-100 dark:text-zinc-900'
                : 'text-zinc-600 hover:bg-zinc-100/80 dark:text-zinc-400 dark:hover:bg-zinc-800'
            "
            @click="activeSettingsTab = 'calls'"
          >
            <svg class="h-3.5 w-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <path d="M4 4h16v16H4z" />
              <path d="M8 9h8M8 13h8M8 17h5" />
            </svg>
            <span>调用记录</span>
          </button>

          <button
            type="button"
            class="flex items-center gap-2 rounded-xl px-4 py-2 text-xs font-medium transition cursor-pointer"
            :class="
              activeSettingsTab === 'about'
                ? 'bg-zinc-900 text-white shadow-sm dark:bg-zinc-100 dark:text-zinc-900'
                : 'text-zinc-600 hover:bg-zinc-100/80 dark:text-zinc-400 dark:hover:bg-zinc-800'
            "
            @click="activeSettingsTab = 'about'"
          >
            <svg class="h-3.5 w-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <circle cx="12" cy="12" r="10" />
              <line x1="12" y1="16" x2="12" y2="12" />
              <line x1="12" y1="8" x2="12.01" y2="8" />
            </svg>
            <span>关于与更新</span>
            <span
              v-if="updateStatus.hasUpdate"
              class="h-2 w-2 rounded-full bg-emerald-500 animate-ping"
            />
          </button>
        </div>

        <!-- 1. 常规与自启 -->
        <div v-show="activeSettingsTab === 'general'" class="space-y-4">
          <!-- 开机自启与后台常驻 -->
          <section
            class="space-y-4 rounded-2xl border border-zinc-200/80 bg-white/85 p-5 shadow-[0_2px_12px_rgba(0,0,0,0.02)] backdrop-blur-xl dark:border-zinc-800 dark:bg-zinc-900/85"
          >
            <div class="flex items-center justify-between border-b border-zinc-100 pb-3.5 dark:border-zinc-800">
              <div>
                <h3 class="text-sm font-semibold">开机自启与后台常驻</h3>
                <p class="text-xs text-zinc-400">
                  配置 Windows 开机静默后台启动与窗口关闭时的托盘常驻行为
                </p>
              </div>
            </div>

            <!-- 开机自启开关 -->
            <div class="flex items-center justify-between rounded-xl border border-zinc-200/70 bg-zinc-50/60 p-3.5 dark:border-zinc-800 dark:bg-zinc-800/40">
              <div>
                <div class="text-xs font-semibold">Windows 登录时自动启动</div>
                <div class="mt-0.5 text-[11px] text-zinc-400">
                  开机登录后自动在后台启动 VPS Switch 并静默常驻系统托盘，无需手动双击运行
                </div>
              </div>
              <label class="inline-flex items-center gap-2 cursor-pointer">
                <span class="text-xs font-medium text-zinc-600 dark:text-zinc-300">
                  {{ autostartEnabled ? '已开启' : '已关闭' }}
                </span>
                <input
                  v-model="autostartEnabled"
                  type="checkbox"
                  class="h-4 w-4 rounded border-zinc-300 text-zinc-900 cursor-pointer"
                  :disabled="savingAutostart"
                  @change="saveAutostartSettings"
                />
              </label>
            </div>

            <!-- 窗口关闭行为 -->
            <div class="flex items-center justify-between rounded-xl border border-zinc-200/70 bg-zinc-50/60 p-3.5 dark:border-zinc-800 dark:bg-zinc-800/40">
              <div>
                <div class="text-xs font-semibold">点击窗口右上角关闭按钮 (✕) 时</div>
                <div class="mt-0.5 text-[11px] text-zinc-400">
                  推荐最小化到系统托盘，保持 SSH 连接池与 MCP Server 在后台随时可用
                </div>
              </div>
              <div class="flex rounded-xl bg-zinc-200/70 p-0.5 text-xs dark:bg-zinc-800">
                <button
                  type="button"
                  class="rounded-lg px-2.5 py-1 font-medium transition cursor-pointer"
                  :class="closeAction === 'tray' ? 'bg-white text-zinc-900 shadow-sm dark:bg-zinc-700 dark:text-zinc-100' : 'text-zinc-500'"
                  @click="closeAction = 'tray'; saveAutostartSettings()"
                >
                  最小化到托盘
                </button>
                <button
                  type="button"
                  class="rounded-lg px-2.5 py-1 font-medium transition cursor-pointer"
                  :class="closeAction === 'exit' ? 'bg-white text-zinc-900 shadow-sm dark:bg-zinc-700 dark:text-zinc-100' : 'text-zinc-500'"
                  @click="closeAction = 'exit'; saveAutostartSettings()"
                >
                  直接退出程序
                </button>
              </div>
            </div>
          </section>

          <!-- 界面与外观主题 -->
          <section
            class="grid gap-3 rounded-2xl border border-zinc-200/80 bg-white/85 p-5 shadow-[0_2px_12px_rgba(0,0,0,0.02)] backdrop-blur-xl sm:grid-cols-2 dark:border-zinc-800 dark:bg-zinc-900/85"
          >
            <div class="flex items-center justify-between rounded-xl border border-zinc-200/70 bg-zinc-50/60 p-3.5 dark:border-zinc-800 dark:bg-zinc-800/40">
              <div>
                <div class="text-xs font-semibold">界面昼夜主题</div>
                <div class="mt-0.5 text-[11px] text-zinc-400">日间白色毛玻璃 / 夜间深空黑毛玻璃</div>
              </div>
              <div class="flex rounded-xl bg-zinc-200/70 p-0.5 text-xs dark:bg-zinc-800">
                <button
                  type="button"
                  class="rounded-lg px-2.5 py-1 font-medium transition cursor-pointer"
                  :class="!isDark ? 'bg-white text-zinc-900 shadow-sm' : 'text-zinc-500'"
                  @click="applyTheme(false)"
                >
                  ☀ 日间
                </button>
                <button
                  type="button"
                  class="rounded-lg px-2.5 py-1 font-medium transition cursor-pointer"
                  :class="isDark ? 'bg-zinc-700 text-white shadow-sm' : 'text-zinc-500'"
                  @click="applyTheme(true)"
                >
                  ☾ 夜间
                </button>
              </div>
            </div>

            <div class="flex items-center justify-between rounded-xl border border-zinc-200/70 bg-zinc-50/60 p-3.5 dark:border-zinc-800 dark:bg-zinc-800/40">
              <div>
                <div class="text-xs font-semibold">终端画布配色</div>
                <div class="mt-0.5 text-[11px] text-zinc-400">独立切换终端区域的白底或黑底</div>
              </div>
              <div class="flex rounded-xl bg-zinc-200/70 p-0.5 text-xs dark:bg-zinc-800">
                <button
                  type="button"
                  class="rounded-lg px-2.5 py-1 font-medium transition cursor-pointer"
                  :class="!terminalDark ? 'bg-white text-zinc-900 shadow-sm' : 'text-zinc-500'"
                  @click="terminalDark = false"
                >
                  白底
                </button>
                <button
                  type="button"
                  class="rounded-lg px-2.5 py-1 font-medium transition cursor-pointer"
                  :class="terminalDark ? 'bg-zinc-700 text-white shadow-sm' : 'text-zinc-500'"
                  @click="terminalDark = true"
                >
                  黑底
                </button>
              </div>
            </div>
          </section>
        </div>

        <!-- 2. MCP Server -->
        <div v-show="activeSettingsTab === 'mcp'" class="space-y-4">
        <!-- 板块 1：MCP Server 开关与 API Key -->
        <section
          class="space-y-4 rounded-2xl border border-zinc-200/80 bg-white/85 p-5 shadow-[0_2px_12px_rgba(0,0,0,0.02)] backdrop-blur-xl dark:border-zinc-800 dark:bg-zinc-900/85"
        >
          <div class="flex items-center justify-between border-b border-zinc-100 pb-3.5 dark:border-zinc-800">
            <div class="flex items-center gap-2.5">
              <span
                class="h-2.5 w-2.5 rounded-full"
                :class="mcpEnabled ? 'bg-emerald-500 shadow-[0_0_8px_rgba(16,185,129,0.6)]' : 'bg-zinc-300 dark:bg-zinc-600'"
              />
              <div>
                <h3 class="text-sm font-semibold">MCP Server 服务与 API Key</h3>
                <p class="text-xs text-zinc-400">
                  支持本机 Stdio 自动接入，或部署在 VPS 上通过 HTTP + API Key 供远程调用
                </p>
              </div>
            </div>

            <!-- MCP 总开关 -->
            <label class="inline-flex items-center gap-2 cursor-pointer">
              <span class="text-xs font-medium text-zinc-600 dark:text-zinc-300">
                {{ mcpEnabled ? '已启用' : '已关闭' }}
              </span>
              <input
                v-model="mcpEnabled"
                type="checkbox"
                class="h-4 w-4 rounded border-zinc-300 text-zinc-900"
                @change="saveMcpSettings(false)"
              />
            </label>
          </div>

          <!-- API Key 鉴权开关与密钥框 -->
          <div class="grid gap-3 md:grid-cols-2">
            <div class="flex items-center justify-between rounded-xl border border-zinc-200/70 bg-zinc-50/60 p-3.5 dark:border-zinc-800 dark:bg-zinc-800/40">
              <div>
                <div class="text-xs font-semibold">远程 HTTP 要求 API Key 鉴权</div>
                <div class="mt-0.5 text-[11px] text-zinc-400">
                  部署在公网 VPS 时请开启，拒绝未携带有效 API Key 的外部请求
                </div>
              </div>
              <input
                v-model="mcpRequireApiKey"
                type="checkbox"
                class="h-4 w-4 shrink-0 rounded border-zinc-300 text-zinc-900 cursor-pointer"
                @change="saveMcpSettings(false)"
              />
            </div>

            <div class="flex flex-col justify-between rounded-xl border border-zinc-200/70 bg-zinc-50/60 p-3.5 dark:border-zinc-800 dark:bg-zinc-800/40">
              <div class="flex items-center justify-between text-xs">
                <span class="font-semibold">MCP API Key</span>
                <div class="flex items-center gap-2">
                  <button
                    type="button"
                    class="text-[11px] text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-200"
                    @click="showApiKey = !showApiKey"
                  >
                    {{ showApiKey ? '隐藏' : '显示' }}
                  </button>
                  <button
                    type="button"
                    class="text-[11px] text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-200"
                    @click="copyText(mcpApiKey, '已复制 MCP API Key')"
                  >
                    复制
                  </button>
                  <button
                    type="button"
                    class="text-[11px] text-orange-600 hover:text-orange-700"
                    :disabled="savingMcp"
                    @click="saveMcpSettings(true)"
                  >
                    重置密钥
                  </button>
                </div>
              </div>
              <div class="mt-1.5 truncate rounded-lg border border-zinc-200/80 bg-white px-2.5 py-1.5 font-mono text-xs text-zinc-800 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200">
                {{ showApiKey ? mcpApiKey : mcpApiKey.replace(/^(.{9}).*(.{4})$/, '$1••••••••••••••••$2') }}
              </div>
            </div>
          </div>

          <!-- 本机 CLI 自动写入状态 -->
          <div class="rounded-xl border border-zinc-200/70 bg-zinc-50/40 p-3.5 dark:border-zinc-800 dark:bg-zinc-800/30">
            <div class="flex items-center justify-between">
              <span class="text-xs font-semibold">本机 Claude Code / Codex 自动写入状态</span>
              <button
                type="button"
                class="rounded-lg border border-zinc-200 bg-white px-2.5 py-1 text-[11px] font-medium text-zinc-700 hover:bg-zinc-50 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
                @click="triggerCliSync"
              >
                立即同步到本机 CLI
              </button>
            </div>
            <div class="mt-2.5 grid grid-cols-2 gap-2.5 text-xs">
              <div class="flex items-center justify-between rounded-lg border border-zinc-200/60 bg-white px-3 py-2 dark:border-zinc-800 dark:bg-zinc-900">
                <div>
                  <div class="font-semibold">Claude Code</div>
                  <div class="font-mono text-[10px] text-zinc-400">~/.claude.json</div>
                </div>
                <span
                  class="rounded px-2 py-0.5 text-[10px] font-medium"
                  :class="
                    mcpStatus?.claudeCodeSynced
                      ? 'bg-emerald-50 text-emerald-700 dark:bg-emerald-950/60 dark:text-emerald-400'
                      : 'bg-zinc-100 text-zinc-500'
                  "
                >
                  {{ mcpStatus?.claudeCodeSynced ? '✓ 已自动写入' : '未检测到' }}
                </span>
              </div>
              <div class="flex items-center justify-between rounded-lg border border-zinc-200/60 bg-white px-3 py-2 dark:border-zinc-800 dark:bg-zinc-900">
                <div>
                  <div class="font-semibold">Codex CLI</div>
                  <div class="font-mono text-[10px] text-zinc-400">~/.codex/config.toml</div>
                </div>
                <span
                  class="rounded px-2 py-0.5 text-[10px] font-medium"
                  :class="
                    mcpStatus?.codexSynced
                      ? 'bg-emerald-50 text-emerald-700 dark:bg-emerald-950/60 dark:text-emerald-400'
                      : 'bg-zinc-100 text-zinc-500'
                  "
                >
                  {{ mcpStatus?.codexSynced ? '✓ 已自动写入' : '未检测到' }}
                </span>
              </div>
            </div>
          </div>

          <!-- 接入配置 JSON 预览（支持切换「远程 HTTP + API Key」与「本机 Stdio」） -->
          <div>
            <div class="mb-2 flex items-center justify-between">
              <div class="flex items-center gap-2">
                <span class="text-xs font-semibold">客户端接入配置预览</span>
                <div class="flex rounded-lg bg-zinc-100 p-0.5 text-[11px] dark:bg-zinc-800">
                  <button
                    type="button"
                    class="rounded-md px-2.5 py-0.5 font-medium transition"
                    :class="
                      configModePreview === 'remote'
                        ? 'bg-white text-zinc-900 shadow-sm dark:bg-zinc-700 dark:text-zinc-100'
                        : 'text-zinc-500'
                    "
                    @click="configModePreview = 'remote'"
                  >
                    远程 HTTP + API Key
                  </button>
                  <button
                    type="button"
                    class="rounded-md px-2.5 py-0.5 font-medium transition"
                    :class="
                      configModePreview === 'stdio'
                        ? 'bg-white text-zinc-900 shadow-sm dark:bg-zinc-700 dark:text-zinc-100'
                        : 'text-zinc-500'
                    "
                    @click="configModePreview = 'stdio'"
                  >
                    本机 Stdio
                  </button>
                </div>
              </div>
              <button
                type="button"
                class="rounded-lg bg-zinc-900 px-3 py-1 text-[11px] font-medium text-white hover:bg-zinc-800 dark:bg-zinc-100 dark:text-zinc-900"
                @click="copyText(displayedMcpConfigJson, '已复制 MCP 配置 JSON')"
              >
                复制配置 JSON
              </button>
            </div>
            <pre
              class="overflow-x-auto rounded-xl border border-zinc-200/80 bg-zinc-950 p-3.5 font-mono text-[11px] leading-relaxed text-zinc-200 dark:border-zinc-800"
            >{{ displayedMcpConfigJson }}</pre>
          </div>
        </section>

        <!-- 板块 1.5：MCP 暴露主机范围 -->
        <section
          class="space-y-4 rounded-2xl border border-zinc-200/80 bg-white/85 p-5 shadow-[0_2px_12px_rgba(0,0,0,0.02)] backdrop-blur-xl dark:border-zinc-800 dark:bg-zinc-900/85"
        >
          <div class="flex items-start justify-between gap-3 border-b border-zinc-100 pb-3.5 dark:border-zinc-800">
            <div>
              <div class="flex items-center gap-2">
                <h3 class="text-sm font-semibold">MCP 暴露主机范围</h3>
                <span class="rounded-full bg-zinc-100 px-2 py-0.5 text-[11px] font-medium text-zinc-600 dark:bg-zinc-800 dark:text-zinc-300">
                  已暴露 {{ mcpExposedCount }} / {{ activeUnarchivedHosts.length }} 台
                </span>
              </div>
              <p class="mt-0.5 text-xs text-zinc-400">
                仅勾选的主机对外部 Agent 可见且可操作；未勾选的主机在 vps_list 中隐藏并拒绝一切 MCP 工具访问
              </p>
            </div>
            <div class="flex shrink-0 items-center gap-1.5">
              <button
                type="button"
                class="rounded-lg border border-zinc-200/80 bg-white px-2.5 py-1 text-[11px] font-medium text-zinc-700 hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
                @click="setAllHostsMcpExposed(true)"
              >
                全选
              </button>
              <button
                type="button"
                class="rounded-lg border border-zinc-200/80 bg-white px-2.5 py-1 text-[11px] font-medium text-zinc-700 hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
                @click="setAllHostsMcpExposed(false)"
              >
                全不选
              </button>
            </div>
          </div>

          <div class="grid max-h-72 grid-cols-1 gap-2 overflow-y-auto sm:grid-cols-2 md:grid-cols-3">
            <button
              v-for="h in activeUnarchivedHosts"
              :key="`mcp-exp-${h.id}`"
              type="button"
              class="flex items-center justify-between gap-2.5 rounded-xl border px-3 py-2 text-left transition"
              :class="
                h.mcpExposed !== false
                  ? 'border-zinc-300 bg-white dark:border-zinc-700 dark:bg-zinc-900'
                  : 'border-zinc-200/70 bg-zinc-50/60 opacity-75 dark:border-zinc-800 dark:bg-zinc-800/40'
              "
              @click="toggleHostMcpExposed(h)"
            >
              <div class="flex min-w-0 items-center gap-2">
                <span
                  class="h-2 w-2 shrink-0 rounded-full"
                  :class="
                    h.status === 'online'
                      ? 'bg-emerald-500'
                      : h.status === 'offline'
                        ? 'bg-red-500'
                        : 'bg-zinc-300 dark:bg-zinc-600'
                  "
                ></span>
                <div class="min-w-0">
                  <div class="truncate text-xs font-semibold">{{ h.name }}</div>
                  <div class="truncate font-mono text-[10px] text-zinc-400">
                    {{ h.kind === 'local' ? `${h.username}@127.0.0.1` : `${h.username}@${h.addr}:${h.port}` }}
                  </div>
                </div>
              </div>
              <input
                type="checkbox"
                :checked="h.mcpExposed !== false"
                class="h-4 w-4 shrink-0 rounded border-zinc-300 text-zinc-900 pointer-events-none"
              />
            </button>
          </div>
        </section>

        <!-- 板块 1.6：MCP 开放工具范围 -->
        <section
          class="space-y-4 rounded-2xl border border-zinc-200/80 bg-white/85 p-5 shadow-[0_2px_12px_rgba(0,0,0,0.02)] backdrop-blur-xl dark:border-zinc-800 dark:bg-zinc-900/85"
        >
          <div class="flex items-start justify-between gap-3 border-b border-zinc-100 pb-3.5 dark:border-zinc-800">
            <div>
              <div class="flex items-center gap-2">
                <h3 class="text-sm font-semibold">MCP 开放工具范围</h3>
                <span class="rounded-full bg-zinc-100 px-2 py-0.5 text-[11px] font-medium text-zinc-600 dark:bg-zinc-800 dark:text-zinc-300">
                  已开放 {{ mcpEnabledTools.length }} / {{ mcpToolCatalog.length }} 个
                </span>
              </div>
              <p class="mt-0.5 text-xs text-zinc-400">
                默认全部开放；取消勾选的工具将从 tools/list 中移除并拒绝外部 Agent 调用（如公网部署仅保留只读巡检）
              </p>
            </div>
            <div class="flex shrink-0 items-center gap-1.5">
              <button
                type="button"
                class="rounded-lg border border-zinc-200/80 bg-white px-2.5 py-1 text-[11px] font-medium text-zinc-700 hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
                @click="setAllMcpTools(true)"
              >
                全选
              </button>
              <button
                type="button"
                class="rounded-lg border border-zinc-200/80 bg-white px-2.5 py-1 text-[11px] font-medium text-zinc-700 hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
                @click="setAllMcpTools(false)"
              >
                全不选
              </button>
            </div>
          </div>

          <div class="grid grid-cols-1 gap-2 sm:grid-cols-2 md:grid-cols-3">
            <button
              v-for="tool in mcpToolCatalog"
              :key="`mcp-tool-${tool.name}`"
              type="button"
              class="flex items-center justify-between gap-2.5 rounded-xl border px-3 py-2 text-left transition"
              :class="
                mcpEnabledTools.includes(tool.name)
                  ? 'border-zinc-300 bg-white dark:border-zinc-700 dark:bg-zinc-900'
                  : 'border-zinc-200/70 bg-zinc-50/60 opacity-75 dark:border-zinc-800 dark:bg-zinc-800/40'
              "
              @click="toggleMcpTool(tool.name)"
            >
              <div class="min-w-0">
                <div class="truncate font-mono text-xs font-semibold">{{ tool.name }}</div>
                <div class="truncate text-[10px] text-zinc-400">{{ tool.desc }}</div>
              </div>
              <input
                type="checkbox"
                :checked="mcpEnabledTools.includes(tool.name)"
                class="h-4 w-4 shrink-0 rounded border-zinc-300 text-zinc-900 pointer-events-none"
              />
            </button>
          </div>
        </section>

        <!-- 板块 2：命令黑名单（Agent 执行护栏） -->
        <section
          class="space-y-4 rounded-2xl border border-zinc-200/80 bg-white/85 p-5 shadow-[0_2px_12px_rgba(0,0,0,0.02)] backdrop-blur-xl dark:border-zinc-800 dark:bg-zinc-900/85"
        >
          <div class="flex items-start justify-between gap-3 border-b border-zinc-100 pb-3.5 dark:border-zinc-800">
            <div>
              <h3 class="text-sm font-semibold">命令黑名单（Agent 执行护栏）</h3>
              <p class="mt-0.5 text-xs text-zinc-400">
                命中即拒绝执行，agent 拿不到任何执行结果。外部 agent 一旦放行 MCP 工具，参数里的命令就不再受它自身权限系统检查，这里是唯一的兜底
              </p>
            </div>
            <button
              type="button"
              class="shrink-0 rounded-lg bg-zinc-900 px-3 py-1 text-[11px] font-medium text-white hover:bg-zinc-800 disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900"
              :disabled="savingGuard"
              @click="saveGuardList"
            >
              {{ savingGuard ? '保存中…' : '保存名单' }}
            </button>
          </div>

          <div class="flex items-center gap-2 rounded-xl border border-zinc-200/70 bg-zinc-50/60 px-3 py-2 dark:border-zinc-800 dark:bg-zinc-800/40">
            <span class="shrink-0 text-[11px] font-medium text-zinc-500">名单文件</span>
            <span class="flex-1 truncate font-mono text-[11px] text-zinc-700 dark:text-zinc-300">{{ guardPath || 'data/deny-list.txt' }}</span>
            <button
              type="button"
              class="shrink-0 text-[11px] text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-200"
              @click="copyText(guardPath, '已复制名单文件路径')"
            >
              复制路径
            </button>
          </div>

          <textarea
            v-model="guardContent"
            spellcheck="false"
            rows="16"
            class="w-full resize-y rounded-xl border border-zinc-200/80 bg-white p-3.5 font-mono text-[11px] leading-relaxed text-zinc-800 outline-none focus:border-zinc-400 dark:border-zinc-700 dark:bg-zinc-950 dark:text-zinc-200"
          ></textarea>

          <p class="text-[11px] leading-relaxed text-zinc-400">
            一行一条，<code class="font-mono">#</code> 起头是注释。<code class="font-mono">*</code> 通配，没有 <code class="font-mono">*</code> 就是精确匹配，按词边界匹配（<code class="font-mono">rm -rf /</code> 不会误伤 <code class="font-mono">rm -rf /var/log</code>）。判定前会剥引号、转小写，并把 <code class="font-mono">-r -f</code> 与 <code class="font-mono">-fr</code> 视作等价。这份名单防手滑，不防蓄意绕过。
          </p>
        </section>
      </div>

      <!-- 分类 3：调用记录 (MCP Call Log) -->
      <div v-show="activeSettingsTab === 'calls'" class="space-y-4">
        <section
          class="space-y-4 rounded-2xl border border-zinc-200/80 bg-white/85 p-5 shadow-[0_2px_12px_rgba(0,0,0,0.02)] backdrop-blur-xl dark:border-zinc-800 dark:bg-zinc-900/85"
        >
          <div class="flex items-start justify-between gap-3 border-b border-zinc-100 pb-3.5 dark:border-zinc-800">
            <div>
              <h3 class="text-sm font-semibold">MCP 调用记录</h3>
              <p class="mt-0.5 text-xs text-zinc-400">
                外部 agent 经 MCP 通道的每一次调用都在此留痕。只记 MCP 这条链，内部执行（探针、压缩、注册表等）不入库。点任意一行展开完整参数与返回结果
              </p>
            </div>
            <button
              type="button"
              class="shrink-0 rounded-lg bg-zinc-900 px-3 py-1 text-[11px] font-medium text-white hover:bg-zinc-800 disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900"
              :disabled="loadingMcpCalls"
              @click="loadMcpCalls"
            >
              {{ loadingMcpCalls ? '刷新中…' : '刷新' }}
            </button>
          </div>

          <!-- 筛选条：时间范围 / 主机 / 工具 / 结果 -->
          <div class="flex flex-wrap items-end gap-2.5">
            <label class="flex flex-col gap-1">
              <span class="text-[11px] font-medium text-zinc-500">起始日期</span>
              <input
                v-model="mcpCallFilters.from"
                type="date"
                class="rounded-xl border border-zinc-200/80 bg-white px-2.5 py-1.5 text-[11px] text-zinc-700 outline-none focus:border-zinc-400 dark:border-zinc-700 dark:bg-zinc-950 dark:text-zinc-200 dark:[color-scheme:dark]"
                @change="loadMcpCalls"
              />
            </label>
            <label class="flex flex-col gap-1">
              <span class="text-[11px] font-medium text-zinc-500">结束日期</span>
              <input
                v-model="mcpCallFilters.to"
                type="date"
                class="rounded-xl border border-zinc-200/80 bg-white px-2.5 py-1.5 text-[11px] text-zinc-700 outline-none focus:border-zinc-400 dark:border-zinc-700 dark:bg-zinc-950 dark:text-zinc-200 dark:[color-scheme:dark]"
                @change="loadMcpCalls"
              />
            </label>
            <label class="flex flex-col gap-1">
              <span class="text-[11px] font-medium text-zinc-500">主机</span>
              <select
                v-model="mcpCallFilters.host"
                class="min-w-[9rem] rounded-xl border border-zinc-200/80 bg-white px-2.5 py-1.5 text-[11px] text-zinc-700 outline-none focus:border-zinc-400 dark:border-zinc-700 dark:bg-zinc-950 dark:text-zinc-200"
                @change="loadMcpCalls"
              >
                <option value="">全部主机</option>
                <option v-for="h in mcpCallHosts" :key="h" :value="h">{{ h }}</option>
              </select>
            </label>
            <label class="flex flex-col gap-1">
              <span class="text-[11px] font-medium text-zinc-500">工具</span>
              <select
                v-model="mcpCallFilters.tool"
                class="min-w-[9rem] rounded-xl border border-zinc-200/80 bg-white px-2.5 py-1.5 text-[11px] text-zinc-700 outline-none focus:border-zinc-400 dark:border-zinc-700 dark:bg-zinc-950 dark:text-zinc-200"
                @change="loadMcpCalls"
              >
                <option value="">全部工具</option>
                <option v-for="t in mcpCallTools" :key="t" :value="t">{{ t }}</option>
              </select>
            </label>
            <label class="flex flex-col gap-1">
              <span class="text-[11px] font-medium text-zinc-500">结果</span>
              <select
                v-model="mcpCallFilters.ok"
                class="rounded-xl border border-zinc-200/80 bg-white px-2.5 py-1.5 text-[11px] text-zinc-700 outline-none focus:border-zinc-400 dark:border-zinc-700 dark:bg-zinc-950 dark:text-zinc-200"
                @change="loadMcpCalls"
              >
                <option value="">全部</option>
                <option value="true">成功</option>
                <option value="false">失败</option>
              </select>
            </label>
            <button
              v-if="mcpCallFiltered"
              type="button"
              class="rounded-xl border border-zinc-200/80 px-3 py-1.5 text-[11px] font-medium text-zinc-500 transition hover:bg-zinc-100/80 cursor-pointer dark:border-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-800"
              @click="resetMcpCallFilters"
            >
              重置筛选
            </button>
          </div>

          <div class="text-[11px] text-zinc-400">
            {{ mcpCallFiltered ? '符合筛选的调用' : '最近调用' }}
            <span class="font-medium text-zinc-600 dark:text-zinc-300">{{ mcpCalls.length }}</span>
            条{{ mcpCalls.length >= MCP_CALL_LIMIT ? `（已达 ${MCP_CALL_LIMIT} 条上限，更早的记录请用筛选查看）` : '' }}
          </div>

          <div
            v-if="mcpCalls.length === 0"
            class="rounded-xl border border-dashed border-zinc-200 py-8 text-center text-xs text-zinc-400 dark:border-zinc-800"
          >
            {{ mcpCallFiltered ? '没有符合筛选条件的调用记录' : '暂无调用记录' }}
          </div>

          <div v-else class="overflow-x-auto">
            <table class="w-full border-collapse text-left text-[11px]">
              <thead>
                <tr class="text-zinc-400">
                  <th class="w-6 py-2"></th>
                  <th class="py-2 pr-3 font-medium whitespace-nowrap">时间</th>
                  <th class="py-2 pr-3 font-medium">工具</th>
                  <th class="py-2 pr-3 font-medium">主机</th>
                  <th class="py-2 pr-3 font-medium">结果</th>
                  <th class="py-2 pr-3 font-medium whitespace-nowrap">耗时</th>
                  <th class="py-2 pr-3 font-medium">参数 / 摘要</th>
                </tr>
              </thead>
              <tbody>
                <template v-for="c in mcpCalls" :key="c.id">
                  <tr
                    class="cursor-pointer border-t border-zinc-100 align-top transition hover:bg-zinc-50/80 dark:border-zinc-800 dark:hover:bg-zinc-800/40"
                    @click="toggleCall(c.id)"
                  >
                    <td class="py-2 pr-1 text-zinc-400 select-none">
                      {{ expandedCallId === c.id ? '▾' : '▸' }}
                    </td>
                    <td class="py-2 pr-3 font-mono whitespace-nowrap text-zinc-500">{{ formatCallTime(c.ts) }}</td>
                    <td class="py-2 pr-3 font-mono text-zinc-800 dark:text-zinc-200">{{ c.tool }}</td>
                    <td class="py-2 pr-3 text-zinc-600 dark:text-zinc-300">{{ c.host || '—' }}</td>
                    <td class="py-2 pr-3">
                      <span
                        class="rounded-md px-1.5 py-0.5 font-medium whitespace-nowrap"
                        :class="
                          c.ok
                            ? 'bg-emerald-50 text-emerald-600 dark:bg-emerald-950/50 dark:text-emerald-400'
                            : 'bg-red-50 text-red-600 dark:bg-red-950/50 dark:text-red-400'
                        "
                      >
                        {{ c.ok ? '成功' : '失败' }}
                      </span>
                    </td>
                    <td class="py-2 pr-3 font-mono whitespace-nowrap text-zinc-500">{{ c.durationMs }}ms</td>
                    <td class="py-2 pr-3">
                      <div class="max-w-[440px] truncate font-mono text-zinc-600 dark:text-zinc-300" :title="c.args">
                        {{ c.args }}
                      </div>
                      <div class="mt-0.5 max-w-[440px] truncate text-zinc-400" :title="c.summary">
                        {{ c.summary }}
                      </div>
                    </td>
                  </tr>

                  <tr
                    v-if="expandedCallId === c.id"
                    class="border-t border-zinc-100 bg-zinc-50/70 dark:border-zinc-800 dark:bg-zinc-950/50"
                  >
                    <td colspan="7" class="px-4 py-3.5">
                      <div v-if="loadingCallDetail" class="text-[11px] text-zinc-400">加载完整内容中…</div>
                      <div v-else class="space-y-3">
                        <div>
                          <div class="mb-1.5 flex items-center gap-2">
                            <span class="text-[11px] font-semibold text-zinc-500">参数（完整）</span>
                            <button
                              type="button"
                              class="rounded-md border border-zinc-200/80 px-2 py-0.5 text-[10px] text-zinc-500 transition hover:bg-zinc-100/80 cursor-pointer dark:border-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-800"
                              @click.stop="copyText(callDetail?.args || '', '已复制完整参数')"
                            >
                              复制
                            </button>
                          </div>
                          <pre
                            class="max-h-72 overflow-auto rounded-xl border border-zinc-200/80 bg-white p-3 font-mono text-[11px] leading-relaxed whitespace-pre-wrap break-all text-zinc-700 dark:border-zinc-700 dark:bg-zinc-950 dark:text-zinc-200"
                          >{{ callDetail?.args || '（无参数）' }}</pre>
                        </div>
                        <div>
                          <div class="mb-1.5 flex items-center gap-2">
                            <span class="text-[11px] font-semibold text-zinc-500">返回结果（完整）</span>
                            <button
                              type="button"
                              class="rounded-md border border-zinc-200/80 px-2 py-0.5 text-[10px] text-zinc-500 transition hover:bg-zinc-100/80 cursor-pointer dark:border-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-800"
                              @click.stop="copyText(callDetail?.result || '', '已复制完整返回结果')"
                            >
                              复制
                            </button>
                          </div>
                          <pre
                            class="max-h-96 overflow-auto rounded-xl border border-zinc-200/80 bg-white p-3 font-mono text-[11px] leading-relaxed whitespace-pre-wrap break-all text-zinc-700 dark:border-zinc-700 dark:bg-zinc-950 dark:text-zinc-200"
                          >{{ callDetail?.result || '（本次调用早于留痕升级，未记录完整返回结果）' }}</pre>
                        </div>
                      </div>
                    </td>
                  </tr>
                </template>
              </tbody>
            </table>
          </div>
        </section>
      </div>

      <!-- 分类 2：访问安全 (Security) -->
      <div v-show="activeSettingsTab === 'security'" class="space-y-4">
        <!-- 板块 2：Web 登录安全与账号密码 -->
        <section
          class="space-y-4 rounded-2xl border border-zinc-200/80 bg-white/85 p-5 shadow-[0_2px_12px_rgba(0,0,0,0.02)] backdrop-blur-xl dark:border-zinc-800 dark:bg-zinc-900/85"
        >
          <div class="flex items-center justify-between border-b border-zinc-100 pb-3.5 dark:border-zinc-800">
            <div>
              <h3 class="text-sm font-semibold">Web 登录鉴权</h3>
              <p class="text-xs text-zinc-400">
                Windows 本机免密直进；VPS 公网部署默认开启密码，防止未授权访问
              </p>
            </div>
            <label class="inline-flex items-center gap-2 cursor-pointer">
              <span class="text-xs font-medium text-zinc-600 dark:text-zinc-300">
                开启密码
              </span>
              <input
                v-model="settingAuthEnabled"
                type="checkbox"
                class="h-4 w-4 rounded border-zinc-300 text-zinc-900"
              />
            </label>
          </div>

          <form class="grid gap-3 sm:grid-cols-[1fr_1fr_auto] sm:items-end" @submit.prevent="saveAuthSettings">
            <div>
              <label class="block text-xs font-medium text-zinc-600 dark:text-zinc-300">管理员账号</label>
              <input
                v-model="settingAdminUsername"
                type="text"
                placeholder="admin"
                class="mt-1 w-full rounded-xl border border-zinc-200 bg-zinc-50/70 px-3 py-2 text-xs outline-none focus:border-zinc-400 focus:bg-white dark:border-zinc-700 dark:bg-zinc-800"
              />
            </div>
            <div>
              <label class="block text-xs font-medium text-zinc-600 dark:text-zinc-300">
                管理员密码
                <span v-if="hasAdminPassword" class="text-zinc-400">（已设置，留空保持不变）</span>
              </label>
              <input
                v-model="settingAdminPassword"
                type="password"
                placeholder="设置或修改登录密码"
                class="mt-1 w-full rounded-xl border border-zinc-200 bg-zinc-50/70 px-3 py-2 text-xs outline-none focus:border-zinc-400 focus:bg-white dark:border-zinc-700 dark:bg-zinc-800"
              />
            </div>
            <button
              type="submit"
              :disabled="savingAuth"
              class="rounded-xl bg-zinc-900 px-4 py-2 text-xs font-medium text-white transition hover:bg-zinc-800 disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900"
            >
              {{ savingAuth ? '保存中...' : '保存登录设置' }}
            </button>
          </form>
        </section>
      </div>

      <!-- 分类 4：关于与更新 (About) -->
      <div v-show="activeSettingsTab === 'about'" class="space-y-4">
        <section
          class="space-y-4 rounded-2xl border border-zinc-200/80 bg-white/85 p-5 shadow-[0_2px_12px_rgba(0,0,0,0.02)] backdrop-blur-xl dark:border-zinc-800 dark:bg-zinc-900/85"
        >
          <div class="flex items-center justify-between border-b border-zinc-100 pb-3.5 dark:border-zinc-800">
            <div class="flex items-center gap-3">
              <BrandLogo size="md" />
              <div>
                <div class="flex items-center gap-2">
                  <h3 class="text-base font-bold">VPS Switch</h3>
                  <span class="rounded-full bg-emerald-100 px-2 py-0.5 font-mono text-[11px] font-semibold text-emerald-800 dark:bg-emerald-950 dark:text-emerald-300">
                    {{ updateStatus.currentVersion }}
                  </span>
                </div>
                <p class="text-xs text-zinc-400">
                  轻量级 VPS 管理工具与 MCP Server · 极简原生 · 零模型调用
                </p>
              </div>
            </div>

            <!-- 检查更新按钮 -->
            <button
              type="button"
              class="inline-flex items-center gap-1.5 rounded-xl bg-zinc-900 px-3.5 py-1.5 text-xs font-semibold text-white shadow-sm transition hover:bg-zinc-800 disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-white cursor-pointer"
              :disabled="updateStatus.checking"
              @click="checkAppUpdate"
            >
              <svg
                class="h-3.5 w-3.5"
                :class="{ 'animate-spin': updateStatus.checking }"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
              >
                <path d="M21 12a9 9 0 1 1-3-6.7" />
                <path d="M21 3v6h-6" />
              </svg>
              <span>{{ updateStatus.checking ? '正在检查...' : '检查更新' }}</span>
            </button>
          </div>

          <!-- 更新检测反馈卡片 -->
          <div
            v-if="updateStatus.checked"
            class="rounded-xl border p-4 transition"
            :class="
              updateStatus.hasUpdate
                ? 'border-emerald-200/80 bg-emerald-50/70 dark:border-emerald-900/60 dark:bg-emerald-950/40'
                : 'border-zinc-200/70 bg-zinc-50/60 dark:border-zinc-800 dark:bg-zinc-800/40'
            "
          >
            <div class="flex items-center justify-between">
              <div class="flex items-center gap-2">
                <span
                  class="h-2 w-2 rounded-full"
                  :class="updateStatus.hasUpdate ? 'bg-emerald-500 animate-ping' : 'bg-zinc-400'"
                />
                <span class="text-xs font-semibold">
                  {{
                    updateStatus.hasUpdate
                      ? `发现新版本 ${updateStatus.latestVersion}`
                      : `当前已是最新版本 (${updateStatus.currentVersion})`
                  }}
                </span>
              </div>
              <a
                v-if="updateStatus.hasUpdate && updateStatus.releaseUrl"
                :href="updateStatus.releaseUrl"
                target="_blank"
                class="text-xs font-semibold text-emerald-600 hover:underline dark:text-emerald-400"
              >
                前往 GitHub 查看发布说明 →
              </a>
            </div>
            <div
              v-if="updateStatus.hasUpdate && updateStatus.releaseNotes"
              class="mt-2.5 rounded-lg border border-emerald-200/60 bg-white/80 p-3 font-mono text-xs text-zinc-700 whitespace-pre-wrap dark:border-emerald-900 dark:bg-zinc-900/80 dark:text-zinc-300"
            >
              {{ updateStatus.releaseNotes }}
            </div>
          </div>

          <!-- 系统运行信息条目 -->
          <div class="grid gap-2.5 text-xs">
            <div class="rounded-xl border border-zinc-200/70 bg-zinc-50/60 p-3 dark:border-zinc-800 dark:bg-zinc-800/40">
              <div class="text-[11px] font-medium text-zinc-400">本地服务监听</div>
              <div class="mt-1 font-mono font-semibold">127.0.0.1:{{ appAbout?.port || 18800 }}</div>
            </div>
            <div class="rounded-xl border border-zinc-200/70 bg-zinc-50/60 p-3 dark:border-zinc-800 dark:bg-zinc-800/40">
              <div class="flex items-center justify-between">
                <div class="text-[11px] font-medium text-zinc-400">可执行文件绝对路径</div>
                <button
                  v-if="appAbout?.exePath"
                  type="button"
                  class="text-[11px] text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-200 cursor-pointer"
                  @click="copyText(appAbout.exePath, '已复制程序绝对路径')"
                >
                  复制路径
                </button>
              </div>
              <div class="mt-1 font-mono text-[11px] truncate text-zinc-600 dark:text-zinc-300">
                {{ appAbout?.exePath || 'vps-switch.exe' }}
              </div>
            </div>
            <div class="flex items-center justify-between rounded-xl border border-zinc-200/70 bg-zinc-50/60 p-3 dark:border-zinc-800 dark:bg-zinc-800/40">
              <div>
                <div class="text-xs font-semibold">GitHub 官方私有仓库</div>
                <div class="mt-0.5 font-mono text-[11px] text-zinc-400">https://github.com/weidu12123/vps-switch</div>
              </div>
              <button
                type="button"
                class="rounded-lg border border-zinc-200 bg-white px-2.5 py-1 text-[11px] font-medium text-zinc-700 hover:bg-zinc-50 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200 cursor-pointer"
                @click="copyText('https://github.com/weidu12123/vps-switch', '已复制 GitHub 仓库链接')"
              >
                复制链接
              </button>
            </div>
          </div>
        </section>
      </div>
      </div>
    </main>

    <!-- 页面 B：主工作台画布（仓库 + 终端/文件 + 右侧等宽栏） -->
    <main v-show="activePage === 'workspace'" class="flex min-h-0 flex-1 items-stretch justify-center overflow-hidden p-4">
      <div
        class="flex h-full min-h-0 w-full items-stretch justify-center transition-all duration-300 ease-[cubic-bezier(0.22,1,0.36,1)]"
        :class="hasCenterStage ? 'max-w-full' : 'max-w-6xl'"
      >
        <!-- 左栏：VPS 仓库 -->
        <section
          class="flex h-full min-h-0 flex-col overflow-hidden transition-all duration-300 ease-[cubic-bezier(0.22,1,0.36,1)]"
          :class="
            hasCenterStage
              ? (showSidebarInStage ? 'w-[336px] shrink-0' : 'w-0 opacity-0 pointer-events-none p-0 overflow-hidden')
              : 'min-w-0 flex-1'
          "
        >
          <div class="flex-1 space-y-2 overflow-y-auto pr-1">
            <div
              v-for="host in filteredHosts"
              :key="host.id"
              :draggable="host.kind !== 'local'"
              class="group relative flex items-center justify-between gap-3 rounded-2xl border px-4 py-3.5 transition-all duration-200 cursor-pointer backdrop-blur-xl"
              :class="[
                stageHostId === host.id || (rightWing === 'detail' && detailHostId === host.id)
                  ? 'border-zinc-900/25 bg-white shadow-[0_4px_20px_rgba(0,0,0,0.05)] ring-1 ring-zinc-900/10 dark:border-zinc-600 dark:bg-zinc-900 dark:ring-zinc-700'
                  : 'border-zinc-200/80 bg-white/82 shadow-[0_1px_4px_rgba(0,0,0,0.02)] hover:border-zinc-300 hover:bg-white/95 dark:border-zinc-800/90 dark:bg-zinc-900/75 dark:hover:border-zinc-700 dark:hover:bg-zinc-900',
                dragOverId === host.id ? 'border-dashed border-orange-400' : '',
              ]"
              @click="onCardClick(host)"
              @dblclick="openTerminalStage(host)"
              @dragstart="onDragStart($event, host)"
              @dragover="onDragOver($event, host)"
              @drop="onDrop($event, host)"
              @dragend="onDragEnd"
            >
              <!-- 左半部 -->
              <div class="flex min-w-0 items-center gap-3">
                <span
                  v-if="!hasCenterStage"
                  class="flex h-6 w-3.5 shrink-0 items-center justify-center text-zinc-300 transition group-hover:text-zinc-400 dark:text-zinc-600 dark:group-hover:text-zinc-500"
                  :class="host.kind === 'local' ? 'opacity-20 cursor-default' : 'cursor-grab'"
                >
                  <svg class="h-4 w-4" viewBox="0 0 24 24" fill="currentColor">
                    <circle cx="9" cy="6" r="1.4" />
                    <circle cx="15" cy="6" r="1.4" />
                    <circle cx="9" cy="12" r="1.4" />
                    <circle cx="15" cy="12" r="1.4" />
                    <circle cx="9" cy="18" r="1.4" />
                    <circle cx="15" cy="18" r="1.4" />
                  </svg>
                </span>

                <OsBadge :os-info="host.osInfo" :kind="host.kind" :name="host.name" :size="hasCenterStage ? 'sm' : 'md'" />

                <div class="min-w-0">
                  <div class="flex items-center gap-2">
                    <span
                      class="h-2.5 w-2.5 shrink-0 rounded-full transition"
                      :class="{
                        'animate-pulse bg-amber-400': checkingIds.has(host.id),
                        'bg-emerald-500 shadow-[0_0_8px_rgba(16,185,129,0.55)]':
                          !checkingIds.has(host.id) && host.status === 'online',
                        'bg-red-500 shadow-[0_0_8px_rgba(239,68,68,0.45)]':
                          !checkingIds.has(host.id) && host.status === 'offline',
                        'bg-zinc-300 dark:bg-zinc-600':
                          !checkingIds.has(host.id) && host.status === 'unknown',
                      }"
                    />
                    <span class="truncate text-sm font-semibold text-zinc-900 dark:text-zinc-100">
                      {{ host.name }}
                    </span>
                    <span
                      v-if="!hasCenterStage && host.pinned && host.kind !== 'local'"
                      class="rounded-md bg-zinc-100 px-1.5 py-0.5 text-[10px] font-medium text-zinc-600 dark:bg-zinc-800 dark:text-zinc-300"
                    >
                      置顶
                    </span>
                    <span
                      v-if="!hasCenterStage && host.role && ROLE_META[host.role]"
                      class="rounded-md border border-zinc-200/80 bg-zinc-50 px-1.5 py-0.5 text-[10px] font-medium text-zinc-500 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-400"
                    >
                      {{ ROLE_META[host.role].label }}
                    </span>
                  </div>
                  <div class="mt-0.5 flex items-center gap-2 truncate font-mono text-[11px] text-zinc-400 dark:text-zinc-500">
                    <span class="truncate">
                      {{
                        host.kind === 'local'
                          ? `${host.username}@127.0.0.1`
                          : `${host.username}@${host.addr}:${host.port}`
                      }}
                    </span>
                    <span v-if="!hasCenterStage && host.jumpHostName" class="text-[11px] text-zinc-400 dark:text-zinc-500">
                      via {{ host.jumpHostName }}
                    </span>
                  </div>
                </div>
              </div>

              <!-- 右半部：卡片上的快捷按钮 -->
              <div class="flex shrink-0 items-center gap-1.5" @click.stop>
                <button
                  type="button"
                  class="rounded-xl border px-2.5 py-1 text-xs font-medium transition"
                  :class="
                    stageHostId === host.id && centerStage === 'terminal'
                      ? 'border-zinc-900 bg-zinc-900 text-white shadow-sm dark:border-zinc-100 dark:bg-zinc-100 dark:text-zinc-900'
                      : 'border-zinc-200/80 bg-white text-zinc-700 hover:border-zinc-300 hover:bg-zinc-900 hover:text-white dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200 dark:hover:bg-zinc-100 dark:hover:text-zinc-900'
                  "
                  @click="openTerminalStage(host)"
                >
                  终端
                </button>
                <button
                  type="button"
                  class="rounded-xl border px-2.5 py-1 text-xs font-medium transition"
                  :class="
                    stageHostId === host.id && centerStage === 'files'
                      ? 'border-zinc-900 bg-zinc-900 text-white shadow-sm dark:border-zinc-100 dark:bg-zinc-100 dark:text-zinc-900'
                      : 'border-zinc-200/80 bg-white text-zinc-700 hover:border-zinc-300 hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200 dark:hover:bg-zinc-700'
                  "
                  @click="openFilesStage(host)"
                >
                  文件
                </button>
                <!-- 收藏按钮（五角星图标 ★） -->
                <button
                  v-if="!hasCenterStage"
                  type="button"
                  class="rounded-xl p-1.5 transition hover:bg-zinc-100 dark:hover:bg-zinc-800"
                  :class="
                    host.favorited
                      ? 'text-amber-500 hover:text-amber-600'
                      : 'text-zinc-400 hover:text-zinc-800 dark:hover:text-zinc-200'
                  "
                  :title="host.favorited ? '取消收藏' : '收藏主机'"
                  @click="handlePatchMeta(host, { favorited: !host.favorited })"
                >
                  <svg
                    class="h-4 w-4"
                    :class="host.favorited ? 'fill-amber-400 text-amber-500' : 'fill-none'"
                    viewBox="0 0 24 24"
                    stroke="currentColor"
                    stroke-width="1.8"
                  >
                    <path d="M12 2l3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01L12 2z" />
                  </svg>
                </button>
                <!-- 置顶按钮（顶部横线 + 向上箭头图标 ⤒） -->
                <button
                  v-if="!hasCenterStage && host.kind !== 'local'"
                  type="button"
                  class="rounded-xl p-1.5 transition hover:bg-zinc-100 dark:hover:bg-zinc-800"
                  :class="
                    host.pinned
                      ? 'bg-zinc-100 text-zinc-900 dark:bg-zinc-800 dark:text-zinc-100'
                      : 'text-zinc-400 hover:text-zinc-800 dark:hover:text-zinc-200'
                  "
                  :title="host.pinned ? '取消置顶' : '置顶到列表顶部'"
                  @click="handlePatchMeta(host, { pinned: !host.pinned })"
                >
                  <svg
                    class="h-4 w-4"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                  >
                    <line x1="5" y1="4" x2="19" y2="4" />
                    <line x1="12" y1="9" x2="12" y2="20" />
                    <polyline points="7 14 12 9 17 14" />
                  </svg>
                </button>
                <button
                  v-if="!hasCenterStage"
                  type="button"
                  class="rounded-xl p-1.5 text-zinc-400 transition hover:bg-zinc-100 hover:text-zinc-800 dark:hover:bg-zinc-800 dark:hover:text-zinc-200"
                  title="编辑主机"
                  @click="openEditModal(host)"
                >
                  <svg class="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8">
                    <path d="M12 20h9" />
                    <path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z" />
                  </svg>
                </button>
              </div>
            </div>

            <div
              v-if="!loading && !filteredHosts.length"
              class="rounded-2xl border border-dashed border-zinc-300/80 bg-white/60 p-10 text-center text-xs text-zinc-400 dark:border-zinc-800 dark:bg-zinc-900/40"
            >
              没有匹配的 VPS 主机
            </div>
          </div>
        </section>

        <!-- 中栏：主舞台 -->
        <div
          class="overflow-hidden transition-all duration-300 ease-[cubic-bezier(0.22,1,0.36,1)]"
          :class="
            hasCenterStage
              ? (showSidebarInStage ? 'ml-3.5 min-w-0 flex-1 opacity-100' : 'ml-0 min-w-0 flex-1 opacity-100')
              : 'ml-0 w-0 flex-none opacity-0 pointer-events-none'
          "
        >
          <template v-for="tab in openTabs" :key="tab.hostId">
            <div
              v-if="hostById(tab.hostId)"
              v-show="centerStage === 'terminal' && stageHostId === tab.hostId"
              class="h-full w-full"
            >
              <TerminalStage
                :host="hostById(tab.hostId)!"
                :panes="tab.panes"
                :split-direction="tab.splitDirection"
                :dark="terminalDark"
                :right-wing="detailHostId === tab.hostId ? rightWing : null"
                :creating="creatingPane"
                :standalone="false"
                :error="tab.error"
                @split="handleSplitTerminal"
                @close-pane="handleClosePane"
                @reconnect-pane="handleReconnectPane"
                @retry="retryPaneForTab(tab)"
                @toggle-dark="terminalDark = !terminalDark"
                @toggle-right-wing="toggleRightWing"
                @collapse-stage="switchToHomeView"
                @meta-update="handlePaneMetaUpdate"
                @popout="handlePopoutTerminal(tab.hostId)"
              />
            </div>
          </template>

          <FileBrowserPanel
            v-if="stageHost && centerStage === 'files'"
            :host="stageHost"
            :compact="false"
            :terminal-cwd="activeSessionTab?.panes[activeSessionTab.panes.length - 1]?.cwd"
            :right-wing="detailHostId === stageHost.id ? rightWing : null"
            @close="switchToHomeView"
            @toggle-detail="toggleRightWing('detail')"
            @session-expired="handleSessionExpired"
            @toast="showToast"
          />
        </div>

        <!-- 最右栏：与左栏完全等宽（336px）的辅助栏 -->
        <div
          class="shrink-0 overflow-hidden transition-all duration-300 ease-[cubic-bezier(0.22,1,0.36,1)]"
          :class="
            rightWing && rightWingHost
              ? 'ml-3.5 w-[336px] opacity-100'
              : 'ml-0 w-0 opacity-0 pointer-events-none'
          "
        >
          <div class="h-full w-[336px]">
            <HostDetailDrawer
              v-if="rightWing === 'detail' && rightWingHost"
              :host="rightWingHost"
              :checking="checkingIds.has(rightWingHost.id)"
              @close="rightWing = null"
              @check="checkSingleHost"
              @edit="openEditModal"
              @delete="handleDeleteHost"
              @open-terminal="openTerminalStage"
              @open-files="openFilesStage"
              @patch-meta="handlePatchMeta"
              @host-updated="replaceHostInList"
            />

            <FileBrowserPanel
              v-else-if="rightWing === 'files' && rightWingHost"
              :host="rightWingHost"
              :compact="true"
              :terminal-cwd="activeSessionTab?.panes[activeSessionTab.panes.length - 1]?.cwd"
              @close="rightWing = null"
              @expand-full="openFilesStage(rightWingHost)"
              @session-expired="handleSessionExpired"
              @toast="showToast"
            />
          </div>
        </div>
      </div>
    </main>

    <!-- 新增 / 编辑主机弹窗 -->
    <HostModal
      :open="modalOpen"
      :editing="editingHost"
      :ssh-hosts="hosts"
      @close="modalOpen = false"
      @submit="handleModalSubmit"
    />

    <!-- 删除主机确认弹窗（替代原生 window.confirm） -->
    <Teleport to="body">
      <div
        v-if="deletingHost"
        class="fixed inset-0 z-50 flex items-center justify-center bg-zinc-900/25 p-4 backdrop-blur-sm"
        @click.self="deletingHost = null"
      >
        <div
          class="w-full max-w-sm rounded-2xl border border-zinc-200/90 bg-white/95 p-5 shadow-2xl backdrop-blur-xl dark:border-zinc-800 dark:bg-zinc-900/95 dark:text-zinc-100"
        >
          <h3 class="text-sm font-semibold">确认删除 VPS 主机</h3>
          <p class="mt-2 text-xs leading-relaxed text-zinc-500 dark:text-zinc-400">
            确定从仓库中删除「<strong class="text-zinc-900 dark:text-zinc-100">{{ deletingHost.name }}</strong>」({{ deletingHost.addr }}) 吗？此操作不可撤销。
          </p>
          <div class="mt-4 flex justify-end gap-2">
            <button
              type="button"
              class="rounded-xl border border-zinc-200 bg-white px-3.5 py-1.5 text-xs font-medium text-zinc-700 hover:bg-zinc-50 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
              @click="deletingHost = null"
            >
              取消
            </button>
            <button
              type="button"
              class="rounded-xl bg-red-600 px-3.5 py-1.5 text-xs font-medium text-white hover:bg-red-700"
              @click="confirmDeleteHost"
            >
              确认删除
            </button>
          </div>
        </div>
      </div>
    </Teleport>

    <!-- 轻量底部 Toast -->
    <Transition
      enter-active-class="transition duration-150 ease-out"
      enter-from-class="opacity-0 -translate-y-2"
      enter-to-class="opacity-100 translate-y-0"
      leave-active-class="transition duration-150 ease-in"
      leave-from-class="opacity-100 translate-y-0"
      leave-to-class="opacity-0 -translate-y-2"
    >
      <div
        v-if="toast"
        class="fixed bottom-5 right-5 z-50 rounded-xl border px-3.5 py-2 text-xs font-medium shadow-lg backdrop-blur-xl"
        :class="
          toast.kind === 'err'
            ? 'border-red-200 bg-white/95 text-red-700 dark:border-red-900 dark:bg-zinc-900/95 dark:text-red-400'
            : 'border-zinc-200 bg-zinc-900/90 text-white dark:border-zinc-700 dark:bg-zinc-100/95 dark:text-zinc-900'
        "
      >
        {{ toast.text }}
      </div>
    </Transition>
  </div>
</template>

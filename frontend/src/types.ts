export interface HostLink {
  id: string;
  name: string;
  url: string;
  description: string;
}

export interface OsInfo {
  os?: string | null;
  distroId?: string | null;
  versionId?: string | null;
  prettyName?: string | null;
  arch?: string | null;
  kernel?: string | null;
  cpuModel?: string | null;
  cpuCores?: number | null;
  detectedAt?: string | null;
}

export type HostRole = 'primary' | 'proxy' | 'test' | 'archive';

export interface PublicHost {
  id: string;
  name: string;
  kind: 'local' | 'ssh';
  addr: string;
  port: number;
  username: string;
  authType: 'local' | 'password' | 'privateKey' | 'none';
  hasPassword: boolean;
  hasPrivateKey: boolean;
  hasPassphrase: boolean;
  jumpHostId?: string | null;
  jumpHostName?: string | null;
  role?: HostRole | null;
  tags: string[];
  links: HostLink[];
  osInfo?: OsInfo | null;
  pinned: boolean;
  favorited?: boolean;
  archived: boolean;
  mcpExposed?: boolean;
  sortOrder: number;
  status: 'online' | 'offline' | 'unknown';
  latencyMs?: number | null;
  lastConnectedAt?: string | null;
  lastError?: string | null;
  createdAt: string;
  updatedAt: string;
}

export interface HostUpsertPayload {
  name: string;
  addr: string;
  port: number;
  username: string;
  authType: 'password' | 'privateKey' | 'none';
  password?: string;
  privateKey?: string;
  passphrase?: string;
  jumpHostId?: string | null;
  role?: string | null;
  tags: string[];
  links: HostLink[];
  pinned: boolean;
  archived: boolean;
  mcpExposed?: boolean;
}

export const ROLE_META: Record<HostRole, { label: string }> = {
  primary: { label: '主力' },
  proxy: { label: '代理' },
  test: { label: '测试' },
  archive: { label: '归档' },
};

export interface PaneMeta {
  id: string;
  hostId: string;
  cols: number;
  rows: number;
  cwd?: string | null;
  title?: string | null;
  running: boolean;
  lastExitCode?: number | null;
  bracketedPaste: boolean;
  exited: boolean;
}

export interface FileEntry {
  name: string;
  path: string;
  isDir: boolean;
  size: number;
  mtime: number;
  permissions?: number | null;
}

export interface TransferTask {
  id: string;
  hostId: string;
  hostName: string;
  direction: 'upload' | 'download';
  filename: string;
  localPath?: string | null;
  remotePath: string;
  totalBytes: number;
  transferredBytes: number;
  speedBps: number;
  state: 'running' | 'completed' | 'failed';
  error?: string | null;
  /** 本次从残片的第几字节接着传（0 = 从头传） */
  resumedFrom: number;
  sha256?: string | null;
  /** true=两端哈希一致；false=不一致（任务已判失败）；null=未校验 */
  verified?: boolean | null;
  startedAt: string;
  endedAt?: string | null;
}

export interface DiskUsageItem {
  mount: string;
  totalBytes: number;
  usedBytes: number;
}

export interface HostInspection {
  hostId: string;
  sampledAt: string;
  uptimeSeconds: number;
  load1: number;
  load5: number;
  load15: number;
  tcpCc?: string | null;
  cpuModel?: string | null;
  cpuCores: number;
  cpuUsagePct: number;
  cpuIowaitPct: number;
  cpuStealPct: number;
  memTotalBytes: number;
  memUsedBytes: number;
  memCachedBytes: number;
  swapTotalBytes: number;
  swapUsedBytes: number;
  diskMount: string;
  diskTotalBytes: number;
  diskUsedBytes: number;
  disks: DiskUsageItem[];
  netIface: string;
  netRxBytesTotal: number;
  netTxBytesTotal: number;
  netRxBps: number;
  netTxBps: number;
}

export const globalInspectionCache = new Map<string, HostInspection>();
export function seedInspectionMap(map: Record<string, HostInspection> | undefined | null) {
  if (!map || typeof map !== 'object') return;
  for (const [k, v] of Object.entries(map)) {
    if (v && typeof v === 'object') {
      globalInspectionCache.set(k, v);
    }
  }
}





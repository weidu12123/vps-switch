<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { ROLE_META, type HostLink, type HostRole, type HostUpsertPayload, type PublicHost } from '../types';

const props = defineProps<{
  open: boolean;
  editing: PublicHost | null;
  sshHosts: PublicHost[];
}>();

const emit = defineEmits<{
  close: [];
  submit: [payload: HostUpsertPayload, editingId: string | null];
}>();

const name = ref('');
const addr = ref('');
const port = ref(22);
const username = ref('root');
const authType = ref<'password' | 'privateKey' | 'none'>('password');
const password = ref('');
const privateKey = ref('');
const passphrase = ref('');
const jumpHostId = ref('');
const role = ref<HostRole | ''>('');
const tagsText = ref('');
const pinned = ref(false);
const archived = ref(false);
const mcpExposed = ref(true);
const links = ref<HostLink[]>([]);
const errorMsg = ref('');

const isEditing = computed(() => Boolean(props.editing?.id));
const isLocal = computed(() => props.editing?.kind === 'local');

const jumpCandidates = computed(() =>
  props.sshHosts.filter((h) => h.kind === 'ssh' && h.id !== props.editing?.id)
);

const roleOptions: Array<{ value: HostRole | ''; label: string }> = [
  { value: '', label: '未标记' },
  { value: 'primary', label: ROLE_META.primary.label },
  { value: 'proxy', label: ROLE_META.proxy.label },
  { value: 'test', label: ROLE_META.test.label },
  { value: 'archive', label: ROLE_META.archive.label },
];

watch(
  () => [props.open, props.editing] as const,
  ([open, host]) => {
    if (!open) return;
    errorMsg.value = '';
    if (host) {
      name.value = host.name;
      addr.value = host.addr;
      port.value = host.port || 22;
      username.value = host.username || 'root';
      authType.value =
        host.authType === 'privateKey'
          ? 'privateKey'
          : host.authType === 'none'
            ? 'none'
            : 'password';
      jumpHostId.value = host.jumpHostId || '';
      role.value = (host.role as HostRole) || '';
      tagsText.value = host.tags.join(', ');
      pinned.value = host.pinned;
      archived.value = host.archived;
      mcpExposed.value = host.mcpExposed !== false;
      links.value = host.links.length
        ? host.links.map((l) => ({ ...l }))
        : [{ id: '', name: '', url: '', description: '' }];
    } else {
      name.value = '';
      addr.value = '';
      port.value = 22;
      username.value = 'root';
      authType.value = 'password';
      jumpHostId.value = '';
      role.value = '';
      tagsText.value = '';
      pinned.value = false;
      archived.value = false;
      mcpExposed.value = true;
      links.value = [{ id: '', name: '', url: '', description: '' }];
    }
    password.value = '';
    privateKey.value = '';
    passphrase.value = '';
  },
  { immediate: true }
);

function addLinkRow() {
  links.value.push({ id: '', name: '', url: '', description: '' });
}

function removeLinkRow(idx: number) {
  links.value.splice(idx, 1);
  if (!links.value.length) {
    links.value.push({ id: '', name: '', url: '', description: '' });
  }
}

function handleSubmit() {
  errorMsg.value = '';
  if (!name.value.trim()) {
    errorMsg.value = '请填写主机名称';
    return;
  }
  if (!isLocal.value && !addr.value.trim()) {
    errorMsg.value = '请填写主机 IP 或域名';
    return;
  }

  const parsedTags = tagsText.value
    .split(/[\s,，、]+/)
    .map((s) => s.trim())
    .filter(Boolean);

  const validLinks = links.value
    .filter((l) => l.name.trim() && l.url.trim())
    .map((l) => ({
      id: l.id || '',
      name: l.name.trim(),
      url: l.url.trim(),
      description: l.description.trim(),
    }));

  const payload: HostUpsertPayload = {
    name: name.value.trim(),
    addr: isLocal.value ? '127.0.0.1' : addr.value.trim(),
    port: Number(port.value) || 22,
    username: username.value.trim() || 'root',
    authType: authType.value,
    jumpHostId: jumpHostId.value || null,
    role: role.value || null,
    tags: parsedTags,
    links: validLinks,
    pinned: pinned.value,
    archived: archived.value,
    mcpExposed: mcpExposed.value,
  };

  if (authType.value === 'password' && password.value) {
    payload.password = password.value;
  } else if (authType.value === 'privateKey') {
    if (privateKey.value.trim()) payload.privateKey = privateKey.value;
    if (passphrase.value) payload.passphrase = passphrase.value;
  }

  emit('submit', payload, props.editing?.id || null);
}
</script>

<template>
  <Teleport to="body">
    <div
      v-if="open"
      class="fixed inset-0 z-50 flex items-center justify-center bg-zinc-900/20 p-4 backdrop-blur-sm"
      @click.self="emit('close')"
    >
      <div
        class="flex max-h-[88vh] w-full max-w-lg flex-col overflow-hidden rounded-2xl border border-zinc-200/90 bg-white/95 shadow-2xl backdrop-blur-xl dark:border-zinc-800 dark:bg-zinc-900/95 dark:text-zinc-100 [&_input]:dark:bg-zinc-800 [&_input]:dark:border-zinc-700 [&_input]:dark:text-zinc-100 [&_select]:dark:bg-zinc-800 [&_select]:dark:border-zinc-700 [&_select]:dark:text-zinc-100 [&_textarea]:dark:bg-zinc-800 [&_textarea]:dark:border-zinc-700 [&_textarea]:dark:text-zinc-100 [&_label]:dark:text-zinc-300"
      >
        <div class="flex items-center justify-between border-b border-zinc-100 px-5 py-3.5 dark:border-zinc-800">
          <h3 class="text-sm font-semibold text-zinc-900 dark:text-zinc-100">
            {{ isEditing ? `编辑主机 · ${editing?.name}` : '新增 VPS 主机' }}
          </h3>
          <button
            type="button"
            class="rounded-lg p-1 text-zinc-400 hover:bg-zinc-100 hover:text-zinc-700"
            @click="emit('close')"
          >
            ✕
          </button>
        </div>

        <form class="flex-1 space-y-3.5 overflow-y-auto px-5 py-4" @submit.prevent="handleSubmit">
          <div
            v-if="errorMsg"
            class="rounded-xl border border-red-200 bg-red-50 px-3 py-2 text-xs text-red-700"
          >
            {{ errorMsg }}
          </div>

          <div class="grid grid-cols-2 gap-3">
            <div>
              <label class="block text-xs font-medium text-zinc-600">主机名称</label>
              <input
                v-model="name"
                type="text"
                placeholder="如 hk-web-1 / 主力机"
                class="mt-1 w-full rounded-xl border border-zinc-200 bg-zinc-50/60 px-3 py-2 text-xs text-zinc-900 outline-none focus:border-zinc-400 focus:bg-white"
              />
            </div>
            <div>
              <label class="block text-xs font-medium text-zinc-600">角色分类</label>
              <select
                v-model="role"
                class="mt-1 w-full rounded-xl border border-zinc-200 bg-zinc-50/60 px-3 py-2 text-xs text-zinc-900 outline-none focus:border-zinc-400 focus:bg-white"
              >
                <option v-for="r in roleOptions" :key="r.value" :value="r.value">
                  {{ r.label }}
                </option>
              </select>
            </div>
          </div>

          <template v-if="!isLocal">
            <div class="grid grid-cols-[1fr_96px_110px] gap-2.5">
              <div>
                <label class="block text-xs font-medium text-zinc-600">IP / 域名</label>
                <input
                  v-model="addr"
                  type="text"
                  placeholder="1.2.3.4 或 example.com"
                  class="mt-1 w-full rounded-xl border border-zinc-200 bg-zinc-50/60 px-3 py-2 font-mono text-xs text-zinc-900 outline-none focus:border-zinc-400 focus:bg-white"
                />
              </div>
              <div>
                <label class="block text-xs font-medium text-zinc-600">端口</label>
                <input
                  v-model.number="port"
                  type="number"
                  min="1"
                  max="65535"
                  class="mt-1 w-full rounded-xl border border-zinc-200 bg-zinc-50/60 px-3 py-2 font-mono text-xs text-zinc-900 outline-none focus:border-zinc-400 focus:bg-white"
                />
              </div>
              <div>
                <label class="block text-xs font-medium text-zinc-600">用户名</label>
                <input
                  v-model="username"
                  type="text"
                  placeholder="root"
                  class="mt-1 w-full rounded-xl border border-zinc-200 bg-zinc-50/60 px-3 py-2 font-mono text-xs text-zinc-900 outline-none focus:border-zinc-400 focus:bg-white"
                />
              </div>
            </div>

            <div class="grid grid-cols-2 gap-3">
              <div>
                <label class="block text-xs font-medium text-zinc-600">认证方式</label>
                <div class="mt-1 grid grid-cols-3 gap-1 rounded-xl bg-zinc-100 p-1">
                  <button
                    type="button"
                    class="rounded-lg py-1.5 text-xs font-medium transition"
                    :class="authType === 'password' ? 'bg-white text-zinc-900 shadow-sm' : 'text-zinc-500'"
                    @click="authType = 'password'"
                  >
                    密码
                  </button>
                  <button
                    type="button"
                    class="rounded-lg py-1.5 text-xs font-medium transition"
                    :class="authType === 'privateKey' ? 'bg-white text-zinc-900 shadow-sm' : 'text-zinc-500'"
                    @click="authType = 'privateKey'"
                  >
                    SSH 私钥
                  </button>
                  <button
                    type="button"
                    class="rounded-lg py-1.5 text-xs font-medium transition"
                    :class="authType === 'none' ? 'bg-white text-zinc-900 shadow-sm' : 'text-zinc-500'"
                    @click="authType = 'none'"
                  >
                    免凭据
                  </button>
                </div>
              </div>
              <div>
                <label class="block text-xs font-medium text-zinc-600">跳板机（可选）</label>
                <select
                  v-model="jumpHostId"
                  class="mt-1 w-full rounded-xl border border-zinc-200 bg-zinc-50/60 px-3 py-2 text-xs text-zinc-900 outline-none focus:border-zinc-400 focus:bg-white"
                >
                  <option value="">直连（不使用跳板）</option>
                  <option v-for="j in jumpCandidates" :key="j.id" :value="j.id">
                    {{ j.name }} ({{ j.addr }}:{{ j.port }})
                  </option>
                </select>
              </div>
            </div>

            <div v-if="authType === 'password'">
              <label class="block text-xs font-medium text-zinc-600">
                SSH 密码
                <span v-if="isEditing && editing?.hasPassword" class="text-zinc-400">
                  （已保存加密密码，留空则保持不变）
                </span>
              </label>
              <input
                v-model="password"
                type="password"
                placeholder="输入 SSH 密码"
                class="mt-1 w-full rounded-xl border border-zinc-200 bg-zinc-50/60 px-3 py-2 font-mono text-xs text-zinc-900 outline-none focus:border-zinc-400 focus:bg-white"
              />
            </div>

            <div v-else-if="authType === 'privateKey'" class="space-y-2.5">
              <div>
                <label class="block text-xs font-medium text-zinc-600">
                  SSH 私钥 (PEM / OpenSSH)
                  <span v-if="isEditing && editing?.hasPrivateKey" class="text-zinc-400">
                    （已保存加密私钥，留空则保持不变）
                  </span>
                </label>
                <textarea
                  v-model="privateKey"
                  rows="3"
                  placeholder="-----BEGIN OPENSSH PRIVATE KEY-----"
                  class="mt-1 w-full rounded-xl border border-zinc-200 bg-zinc-50/60 px-3 py-2 font-mono text-xs text-zinc-900 outline-none focus:border-zinc-400 focus:bg-white"
                />
              </div>
              <div>
                <label class="block text-xs font-medium text-zinc-600">私钥口令 Passphrase（可选）</label>
                <input
                  v-model="passphrase"
                  type="password"
                  placeholder="无口令可留空"
                  class="mt-1 w-full rounded-xl border border-zinc-200 bg-zinc-50/60 px-3 py-2 font-mono text-xs text-zinc-900 outline-none focus:border-zinc-400 focus:bg-white"
                />
              </div>
            </div>
          </template>

          <div>
            <label class="block text-xs font-medium text-zinc-600">标签（逗号或空格分隔）</label>
            <input
              v-model="tagsText"
              type="text"
              placeholder="如 hk, bgp, docker"
              class="mt-1 w-full rounded-xl border border-zinc-200 bg-zinc-50/60 px-3 py-2 text-xs text-zinc-900 outline-none focus:border-zinc-400 focus:bg-white"
            />
          </div>

          <div>
            <div class="flex items-center justify-between">
              <label class="text-xs font-medium text-zinc-600">快捷链接（商家后台 / 面板）</label>
              <button
                type="button"
                class="text-xs font-medium text-zinc-600 hover:text-zinc-900"
                @click="addLinkRow"
              >
                + 添加一行
              </button>
            </div>
            <div class="mt-1.5 space-y-1.5">
              <div
                v-for="(link, idx) in links"
                :key="idx"
                class="grid grid-cols-[110px_1fr_28px] gap-1.5"
              >
                <input
                  v-model="link.name"
                  type="text"
                  placeholder="名称"
                  class="rounded-lg border border-zinc-200 bg-zinc-50/60 px-2.5 py-1.5 text-xs outline-none focus:border-zinc-400 focus:bg-white"
                />
                <input
                  v-model="link.url"
                  type="text"
                  placeholder="https://..."
                  class="rounded-lg border border-zinc-200 bg-zinc-50/60 px-2.5 py-1.5 font-mono text-xs outline-none focus:border-zinc-400 focus:bg-white"
                />
                <button
                  type="button"
                  class="rounded-lg text-zinc-400 hover:bg-zinc-100 hover:text-zinc-700"
                  @click="removeLinkRow(idx)"
                >
                  ×
                </button>
              </div>
            </div>
          </div>

          <div class="flex items-center gap-5 pt-1">
            <label v-if="!isLocal" class="inline-flex items-center gap-2 text-xs text-zinc-700">
              <input v-model="pinned" type="checkbox" class="rounded border-zinc-300 text-zinc-900" />
              置顶显示
            </label>
            <label v-if="!isLocal" class="inline-flex items-center gap-2 text-xs text-zinc-700">
              <input v-model="archived" type="checkbox" class="rounded border-zinc-300 text-zinc-900" />
              归档隐藏
            </label>
            <label class="inline-flex items-center gap-2 text-xs text-zinc-700">
              <input v-model="mcpExposed" type="checkbox" class="rounded border-zinc-300 text-zinc-900" />
              允许 MCP 访问
            </label>
          </div>
        </form>

        <div class="flex items-center justify-end gap-2 border-t border-zinc-100 bg-zinc-50/60 px-5 py-3">
          <button
            type="button"
            class="rounded-xl border border-zinc-200 bg-white px-4 py-1.5 text-xs font-medium text-zinc-700 hover:bg-zinc-50"
            @click="emit('close')"
          >
            取消
          </button>
          <button
            type="button"
            class="rounded-xl bg-zinc-900 px-4 py-1.5 text-xs font-medium text-white hover:bg-zinc-800"
            @click="handleSubmit"
          >
            保存
          </button>
        </div>
      </div>
    </div>
  </Teleport>
</template>

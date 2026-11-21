import { markRaw, nextTick, ref, shallowRef, type Component, type Ref } from 'vue'

/**
 * 首次使用时才加载的命令式对话框（经 ref 调用 open 等方法）。
 * 模板里用 `<component :is="component" v-if="component" ref="…">` 挂载；
 * `ensure()` 负责加载、挂载并返回组件实例，缩小首屏包体。
 */
export function lazyDialog<T>(loader: () => Promise<{ default: Component }>) {
  const component = shallowRef<Component>()
  const instance = ref() as Ref<T | undefined>
  let loading: Promise<T> | undefined

  function ensure(): Promise<T> {
    if (instance.value) return Promise.resolve(instance.value)
    loading ??= loader()
      .then(async (mod) => {
        component.value = markRaw(mod.default)
        await nextTick()
        if (!instance.value) throw new Error('对话框加载失败')
        return instance.value
      })
      .catch((error) => {
        loading = undefined
        throw error
      })
    return loading
  }

  return { component, instance, ensure }
}

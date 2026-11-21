import { inject, type InjectionKey } from 'vue'
import type { TransferController } from './controller'
import type { UploadIntake } from './intake'
export const TRANSFERS: InjectionKey<TransferController> = Symbol('传输控制器')
export const UPLOAD_INTAKE: InjectionKey<UploadIntake> = Symbol('上传入口')
export function useTransfers() {
  const value = inject(TRANSFERS)
  if (!value) throw new Error('传输功能需要登录会话')
  return value
}
export function useUploadIntake() {
  const value = inject(UPLOAD_INTAKE)
  if (!value) throw new Error('上传功能需要登录会话')
  return value
}

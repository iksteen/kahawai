import { json } from './api.ts'

export const apiClient = <T>(url: string, options: RequestInit): Promise<T> => json<T>(url, options)

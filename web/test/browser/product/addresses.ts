function address(name: string, fallback: string): string {
  return process.env[name] ?? fallback
}

export const PUBLIC_ADDRESS = address('KAHAWAI_E2E_PUBLIC', '127.0.0.1:18430')
export const SETUP_ADDRESS = address('KAHAWAI_E2E_SETUP', '127.0.0.1:18431')
export const SATELLITE_ADDRESS = address('KAHAWAI_E2E_SATELLITE', '127.0.0.1:18432')
export const CONTROL_ADDRESS = address('KAHAWAI_E2E_CONTROL', '127.0.0.1:18433')

export const PUBLIC = `http://${PUBLIC_ADDRESS}`
export const SETUP = `http://${SETUP_ADDRESS}`
export const CONTROL = `http://${CONTROL_ADDRESS}`

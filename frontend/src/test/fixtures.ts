import type { DirectoryEntry } from '../api'

export function entry(id: number, modified_at = 1700000000): DirectoryEntry {
  return { id, name: `${id}.jpg`, is_dir: false, size: 100, created_at: 1, modified_at, sha256: null, favorite: false, has_thumbnail: false }
}

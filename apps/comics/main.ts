import { startApp } from '../reader/app'
import { ComicReader } from './comic'
void startApp({ kind: 'comics', create: async (context) => new ComicReader(context) })
  .catch((error) => { document.getElementById('app')!.textContent = `漫画打开失败：${error.message}` })

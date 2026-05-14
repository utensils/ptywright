import DefaultTheme from 'vitepress/theme'
import { h } from 'vue'
import PlatformStrip from './platform-strip.vue'
import './style.css'

export default {
  extends: DefaultTheme,
  Layout() {
    return h(DefaultTheme.Layout, null, {
      'home-hero-info-after': () => h(PlatformStrip),
    })
  },
}

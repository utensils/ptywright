import DefaultTheme from 'vitepress/theme'
import type { Theme } from 'vitepress'
import OperatorHome from './OperatorHome.vue'
import RuntimeSchematic from './RuntimeSchematic.vue'
import './style.css'

const theme: Theme = {
  extends: DefaultTheme,
  enhanceApp({ app }) {
    app.component('OperatorHome', OperatorHome)
    app.component('RuntimeSchematic', RuntimeSchematic)
  },
}

export default theme

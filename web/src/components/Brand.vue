<script setup lang="ts">
// The supplied wordmark is outlined: Outfit is part of the SVG, so the
// branding renders without a font download or a network dependency.
import lockup from '../assets/brand/kahawai-lockup-for-dark-bg.svg'
</script>

<template>
  <img :src="lockup" alt="Kahawai" width="231" height="42" />
</template>

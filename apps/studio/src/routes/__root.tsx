import { Outlet, createRootRoute } from '@tanstack/react-router'

import { StudioDevtools } from '@/components/studio-devtools'

import '../styles.css'

export const Route = createRootRoute({
  component: RootComponent,
})

function RootComponent() {
  return (
    <>
      <Outlet />
      <StudioDevtools />
    </>
  )
}

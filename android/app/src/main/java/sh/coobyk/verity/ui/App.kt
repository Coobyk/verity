package sh.coobyk.verity.ui

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.SizeTransform
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.lifecycle.compose.LifecycleResumeEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import sh.coobyk.verity.data.TdRepository

private enum class Nav(val depth: Int) { Login(0), List(1), Settings(2), Chat(2) }

@Composable
fun VerityApp(vm: AppViewModel) {
    val ui by vm.ui.collectAsStateWithLifecycle()
    val conv by vm.conversation.collectAsStateWithLifecycle()
    val settingsOpen by vm.showSettings.collectAsStateWithLifecycle()
    val auth by vm.authState.collectAsStateWithLifecycle()
    val snackbar = remember { SnackbarHostState() }

    // Refresh chat lists when the app is resumed.
    LifecycleResumeEffect(Unit) {
        vm.setForeground(true)
        onPauseOrDispose { vm.setForeground(false) }
    }

    LaunchedEffect(ui.status) {
        val msg = ui.status
        if (msg != null) {
            snackbar.showSnackbar(msg)
            vm.clearStatus()
        }
    }

    Box(
        Modifier
            .fillMaxSize()
            .background(VerityBlack)
            .statusBarsPadding()
            .navigationBarsPadding(),
    ) {
        val target = when {
            auth !is TdRepository.AuthState.Ready -> Nav.Login
            settingsOpen -> Nav.Settings
            conv != null -> Nav.Chat
            else -> Nav.List
        }
        AnimatedContent(
            targetState = target,
            transitionSpec = {
                val forward = targetState.depth > initialState.depth
                val inSpec =
                    slideInHorizontally { if (forward) it / 4 else -it / 4 } +
                        fadeIn()
                val outSpec =
                    slideOutHorizontally { if (forward) -it / 4 else it / 4 } +
                        fadeOut()
                inSpec togetherWith outSpec using
                    SizeTransform(clip = false)
            },
            label = "nav",
        ) { t ->
            when (t) {
                Nav.Login -> LoginScreen(vm)
                Nav.Settings -> SettingsScreen(vm)
                Nav.Chat -> ChatScreen(vm, conv!!)
                Nav.List -> ListScreen(vm)
            }
        }
        SnackbarHost(
            hostState = snackbar,
            modifier = Modifier.align(Alignment.BottomCenter),
        )
    }
}

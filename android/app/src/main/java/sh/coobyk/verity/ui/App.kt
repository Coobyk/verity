package sh.coobyk.verity.ui

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.SizeTransform
import androidx.compose.animation.core.ExperimentalTransitionApi
import androidx.compose.animation.core.SeekableTransitionState
import androidx.compose.animation.core.rememberTransition
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
import sh.coobyk.verity.data.Conversation
import sh.coobyk.verity.data.TdRepository

/**
 * Nav state. Chat carries its Conversation so the screen keeps rendering
 * valid data while it animates out — a nullable `conv` lookup would NPE
 * mid-transition.
 */
private sealed interface Screen {
    val depth: Int

    data object Login : Screen { override val depth = 0 }
    data object List : Screen { override val depth = 1 }
    data object Settings : Screen { override val depth = 2 }
    data class Chat(val conv: Conversation) : Screen { override val depth = 2 }
}

@OptIn(ExperimentalTransitionApi::class)
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
        val c = conv
        val target: Screen = when {
            auth !is TdRepository.AuthState.Ready -> Screen.Login
            settingsOpen -> Screen.Settings
            c != null -> Screen.Chat(c)
            else -> Screen.List
        }
        val transitionState = remember { SeekableTransitionState(target) }
        val transition = rememberTransition(transitionState, label = "nav")

        // Predictive back: while a gesture drags, seek the transition
        // toward the list; on release the screens commit (vm.back()) or
        // cancel (progress back to -1) and we animate to the real target.
        val back by vm.backProgress.collectAsStateWithLifecycle()
        LaunchedEffect(target, back) {
            if (back >= 0f) transitionState.seekTo(back, Screen.List)
            else transitionState.animateTo(target)
        }

        transition.AnimatedContent(
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
        ) { screen ->
            when (screen) {
                is Screen.Login -> LoginScreen(vm)
                is Screen.Settings -> SettingsScreen(vm)
                is Screen.Chat -> ChatScreen(vm, screen.conv)
                is Screen.List -> ListScreen(vm)
            }
        }
        SnackbarHost(
            hostState = snackbar,
            modifier = Modifier.align(Alignment.BottomCenter),
        )
    }
}

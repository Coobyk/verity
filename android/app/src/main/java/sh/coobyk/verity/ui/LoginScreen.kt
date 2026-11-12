package sh.coobyk.verity.ui

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextField
import androidx.compose.material3.TextFieldDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import sh.coobyk.verity.data.TdRepository

/**
 * Telegram login via TDLib: phone → code → optional 2FA password.
 * Driven entirely by the repo's auth state machine; styling matches the
 * rest of the app (pure black, pill inputs, inverted pill button).
 */
@Composable
fun LoginScreen(vm: AppViewModel) {
    val state by vm.authState.collectAsStateWithLifecycle()
    val error by vm.authError.collectAsStateWithLifecycle()
    val busy by vm.authBusy.collectAsStateWithLifecycle()

    Box(
        Modifier
            .fillMaxSize()
            .imePadding(),
        contentAlignment = Alignment.Center,
    ) {
        Column(
            Modifier
                .widthIn(max = 380.dp)
                .fillMaxWidth()
                .padding(horizontal = 28.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text(
                "Verity",
                color = MainText,
                fontSize = 28.sp,
                fontWeight = FontWeight.Bold,
            )
            Spacer(Modifier.height(6.dp))
            Text(
                subtitleFor(state),
                color = DimText,
                fontSize = 14.sp,
                textAlign = TextAlign.Center,
            )
            Spacer(Modifier.height(28.dp))

            when (state) {
                is TdRepository.AuthState.Starting -> {
                    CircularProgressIndicator(
                        color = DimText,
                        modifier = Modifier.height(28.dp),
                        strokeWidth = 2.dp,
                    )
                }
                is TdRepository.AuthState.WaitPhone -> LoginStep(
                    placeholder = "+1 555 123 4567",
                    keyboardType = KeyboardType.Phone,
                    secret = false,
                    busy = busy,
                    button = "Send code",
                    onSubmit = vm::sendPhone,
                )
                is TdRepository.AuthState.WaitCode -> LoginStep(
                    placeholder = "Code",
                    keyboardType = KeyboardType.NumberPassword,
                    secret = true,
                    busy = busy,
                    button = "Verify",
                    onSubmit = vm::sendCode,
                )
                is TdRepository.AuthState.WaitPassword -> LoginStep(
                    placeholder = "Two-step password",
                    keyboardType = KeyboardType.Password,
                    secret = true,
                    busy = busy,
                    button = "Unlock",
                    onSubmit = vm::sendPassword,
                )
                is TdRepository.AuthState.WaitOtherDevice -> {
                    Text(
                        "Confirm this login in the Telegram app on another device.",
                        color = DimText,
                        fontSize = 14.sp,
                        textAlign = TextAlign.Center,
                    )
                }
                is TdRepository.AuthState.Ready -> {}
            }

            val err = error
            if (err != null) {
                Spacer(Modifier.height(14.dp))
                Text(
                    err,
                    color = ErrorRed,
                    fontSize = 13.sp,
                    textAlign = TextAlign.Center,
                )
            }
        }
    }
}

private fun subtitleFor(state: TdRepository.AuthState): String = when (state) {
    is TdRepository.AuthState.Starting -> "Connecting to Telegram…"
    is TdRepository.AuthState.WaitPhone -> "Sign in with your Telegram phone number"
    is TdRepository.AuthState.WaitCode -> "Enter the code Telegram sent you"
    is TdRepository.AuthState.WaitPassword -> "Your account has two-step verification"
    is TdRepository.AuthState.WaitOtherDevice -> "Check your other devices"
    is TdRepository.AuthState.Ready -> ""
}

@Composable
private fun LoginStep(
    placeholder: String,
    keyboardType: KeyboardType,
    secret: Boolean,
    busy: Boolean,
    button: String,
    onSubmit: (String) -> Unit,
) {
    var text by remember { mutableStateOf("") }

    fun submit() {
        if (!busy && text.isNotBlank()) onSubmit(text)
    }

    // Pill-shaped input, same look as the chat composer.
    Surface(
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(28.dp),
        color = ComposerFill,
        border = BorderStroke(0.5.dp, Hairline),
    ) {
        TextField(
            value = text,
            onValueChange = { text = it },
            enabled = !busy,
            placeholder = { Text(placeholder, color = DimText, fontSize = 15.sp) },
            singleLine = true,
            textStyle = LocalTextStyle.current.copy(
                color = MainText,
                fontSize = 15.sp,
            ),
            visualTransformation =
                if (secret) PasswordVisualTransformation()
                else VisualTransformation.None,
            colors = TextFieldDefaults.colors(
                focusedTextColor = MainText,
                unfocusedTextColor = MainText,
                disabledTextColor = DimText,
                focusedContainerColor = Color.Transparent,
                unfocusedContainerColor = Color.Transparent,
                disabledContainerColor = Color.Transparent,
                cursorColor = MainText,
                focusedIndicatorColor = Color.Transparent,
                unfocusedIndicatorColor = Color.Transparent,
                disabledIndicatorColor = Color.Transparent,
            ),
            keyboardOptions = KeyboardOptions(
                keyboardType = keyboardType,
                imeAction = ImeAction.Done,
            ),
            keyboardActions = KeyboardActions(onDone = { submit() }),
        )
    }
    Spacer(Modifier.height(14.dp))
    Button(
        onClick = { submit() },
        enabled = !busy && text.isNotBlank(),
        shape = RoundedCornerShape(24.dp),
        colors = ButtonDefaults.buttonColors(
            containerColor = MainText,
            contentColor = VerityBlack,
        ),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Text(
            if (busy) "…" else button,
            fontWeight = FontWeight.SemiBold,
        )
    }
}
